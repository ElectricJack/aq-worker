//! The worker-side aq-git child, a port of the exchange/stop steps of aq-server's
//! `scripts/ms1-node-run.py`.
use crate::config::{AqGitConfig, RepositoryBinding};
use anyhow::{Context, Result, bail, ensure};
use std::{
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};
use uuid::Uuid;

pub struct AqGit {
    child: Option<Child>,
    /// The leader's pid, taken at spawn: `Child::id` is gone once the leader is reaped, and
    /// the group (the engine) may outlive it.
    leader: Option<u32>,
    socket: PathBuf,
}
/// What a SIGKILLed aq-git leaves behind (a crash, or `systemctl kill`): its socket, which a
/// new aq-git cannot bind, and its ownership fence (`incarnation.json` still active), which
/// makes a new aq-git refuse the state with `recovery_blocked`. Both are cleared only when no
/// process holds the state's `owner.lock`: aq-git's guard and engine hold it until they and
/// every descendant are gone.
async fn reclaim(config: &AqGitConfig, git_dir: &Path, socket: &Path) -> Result<()> {
    if let Ok(found) = std::fs::symlink_metadata(socket) {
        ensure!(
            found.file_type().is_socket(),
            "{} is not a socket; refusing to replace it",
            socket.display()
        );
        let probe = tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(socket));
        match probe.await {
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(Ok(_)) => bail!("a live aq-git still listens on {}", socket.display()),
            _ => bail!(
                "{} neither accepts nor refuses a connection; kept",
                socket.display()
            ),
        }
        ensure!(
            owner_gone(git_dir).await?,
            "the aq-git state {} is still owned by a live process; its socket {} is kept",
            git_dir.display(),
            socket.display()
        );
        let again = std::fs::symlink_metadata(socket)?;
        ensure!(
            (again.dev(), again.ino()) == (found.dev(), found.ino()),
            "{} changed while it was checked",
            socket.display()
        );
        std::fs::remove_file(socket).with_context(|| format!("remove {}", socket.display()))?;
        tracing::warn!(socket = %socket.display(), "removed the socket of a killed aq-git");
    }
    clear_fence(config, git_dir).await
}
/// True when no process holds `<git_dir>/owner.lock` (or it does not exist). Tried for up to
/// 250 ms: a child forked elsewhere in this process holds a copy of a lock descriptor until it
/// execs.
async fn owner_gone(git_dir: &Path) -> Result<bool> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(git_dir.join("owner.lock"));
    let lock = match lock {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        other => other.context("open the aq-git owner lock")?,
    };
    for _ in 0..25 {
        // SAFETY: a non-blocking flock on our own descriptor; released when `lock` drops.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(false)
}
/// Clears the ownership fence of a killed aq-git with `recovery clear-fence`, attesting the
/// free owner lock as the evidence that the old environment stopped.
async fn clear_fence(config: &AqGitConfig, git_dir: &Path) -> Result<()> {
    let Ok(bytes) = std::fs::read(git_dir.join("incarnation.json")) else {
        return Ok(());
    };
    let record: serde_json::Value =
        serde_json::from_slice(&bytes).context("decode aq-git incarnation.json")?;
    if record["active"] != true {
        return Ok(());
    }
    let incarnation = record["id"].as_str().context("aq-git incarnation id")?;
    ensure!(
        owner_gone(git_dir).await?,
        "the aq-git state {} is still owned by a live process",
        git_dir.display()
    );
    let id = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "protocol_version": 1, "request_id": id, "operation_id": id,
        "operation": "recovery.clear-fence",
        "params": {"expected_incarnation": incarnation, "old_environment_stopped": true,
            "reason": "aq-worker service: the previous aq-git of this slot was killed",
            "evidence": {"owner_lock": "free", "guard_pid": record["guard_pid"],
                "boot_id": record["boot_id"]}}});
    let mut child = Command::new(&config.binary)
        .arg("--state-dir")
        .arg(git_dir)
        .arg("--git-executable")
        .arg(&config.git_executable)
        .args(["--json", "--input", "-", "recovery", "clear-fence"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", config.binary.display()))?;
    let mut stdin = child.stdin.take().context("aq-git stdin")?;
    stdin.write_all(&serde_json::to_vec(&request)?).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .context("aq-git recovery clear-fence did not finish")??;
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    ensure!(
        out.status.success() && reply["ok"] == true,
        "aq-git refused to clear the fence of its killed predecessor ({}): {}",
        out.status,
        brief(&reply["error"])
    );
    tracing::warn!(
        incarnation,
        "cleared the ownership fence of a killed aq-git (no process held its owner lock)"
    );
    Ok(())
}
fn brief(v: &serde_json::Value) -> String {
    v.to_string().chars().take(500).collect()
}
fn matches(binding: &serde_json::Value, repo: &RepositoryBinding) -> bool {
    binding["mode"] == "worker"
        && binding["identity"] == repo.identity.as_str()
        && binding["binding_generation"] == repo.binding_generation
        && binding["object_format"] == repo.object_format.as_str()
}
impl AqGit {
    /// Spawns `<binary> --state-dir <dir>/git --listen <socket> --git-executable <git> serve`
    /// (stdio null, own process group) after `reclaim`, and waits up to 10 s until the socket
    /// accepts a connection.
    pub async fn start(config: &AqGitConfig, state_dir: &Path, socket: &Path) -> Result<Self> {
        let git_dir = state_dir.join("git");
        reclaim(config, &git_dir, socket).await?;
        let mut child = Command::new(&config.binary)
            .arg("--state-dir")
            .arg(&git_dir)
            .arg("--listen")
            .arg(socket)
            .arg("--git-executable")
            .arg(&config.git_executable)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {}", config.binary.display()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(socket).await.is_err() {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "worker-side aq-git exited ({status}) before it listened on {}",
                    socket.display()
                );
            }
            if tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                bail!("worker-side aq-git did not listen");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self {
            leader: child.id(),
            child: Some(child),
            socket: socket.to_path_buf(),
        })
    }
    #[cfg(test)]
    pub(crate) fn over_socket(socket: PathBuf) -> Self {
        Self {
            child: None,
            leader: None,
            socket,
        }
    }
    /// repo.register for each binding (expected_binding_generation 0). A repository the state
    /// already holds (a recovery restarts aq-git on the attempt's state) is answered
    /// `binding_conflict` "Repository already registered": then its registration is read with
    /// repo.show and accepted when it is this binding. Refuses unless the binding matches.
    pub async fn register(&self, repos: &[RepositoryBinding]) -> Result<()> {
        for repo in repos {
            let id = Uuid::new_v4().to_string();
            let request = serde_json::json!({
                "protocol_version": 1, "request_id": id, "operation_id": id,
                "operation": "repo.register", "repository_id": repo.repository_id,
                "expected_binding_generation": 0,
                "params": {"binding": {"identity": repo.identity, "mode": "worker",
                    "binding_generation": repo.binding_generation, "object_format": repo.object_format}}});
            let reply = self.exchange(&request).await?;
            let error = &reply["error"];
            let binding = if reply["ok"] == true && reply["request_id"] == id.as_str() {
                reply["result"].clone()
            } else if error["code"] == "binding_conflict"
                && error["message"] == "Repository already registered"
            {
                self.show(repo).await?
            } else {
                bail!("aq-git refused repo.register: {}", brief(error));
            };
            ensure!(
                matches(&binding, repo),
                "worker binding mismatch for repository {}: {}",
                repo.repository_id,
                brief(&binding)
            );
        }
        Ok(())
    }
    async fn show(&self, repo: &RepositoryBinding) -> Result<serde_json::Value> {
        let id = Uuid::new_v4().to_string();
        let request = serde_json::json!({
            "protocol_version": 1, "request_id": id, "operation": "repo.show",
            "repository_id": repo.repository_id,
            "expected_binding_generation": repo.binding_generation, "params": {}});
        let reply = self.exchange(&request).await?;
        ensure!(
            reply["ok"] == true && reply["request_id"] == id.as_str(),
            "aq-git refused repo.show of the registered repository {}: {}",
            repo.repository_id,
            brief(&reply["error"])
        );
        Ok(reply["result"].clone())
    }
    async fn exchange(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .context("connect to aq-git")?;
            stream
                .write_all(&[serde_json::to_vec(request)?, b"\n".to_vec()].concat())
                .await?;
            let mut line = Vec::new();
            BufReader::new(stream.take(65537))
                .read_until(b'\n', &mut line)
                .await?;
            ensure!(line.len() <= 65536, "aq-git reply too large");
            serde_json::from_slice(&line).context("decode aq-git reply")
        })
        .await
        .context("aq-git did not answer")?
    }
    /// SIGTERM, wait 10 s, then SIGKILL to its whole process group: aq-git's engine is a
    /// forked child that holds the state's owner lock and the socket.
    pub async fn stop(mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = self.leader;
        if let Some(pid) = child.id() {
            // SAFETY: plain signal to our own child.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        let stopped = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        // Whatever remains of the group (an engine that outlived its guard) holds the owner
        // lock and the socket: SIGKILL the whole group, on a graceful exit too.
        kill_group(pid);
        if stopped.is_err() {
            child.kill().await?;
        }
        Ok(())
    }
}
/// SIGKILL to the process group `pid` leads (`process_group(0)`); ESRCH when it is gone.
/// Skipped when the group is already gone (the pid-recycle window is accepted).
fn kill_group(pid: Option<u32>) {
    if let Some(group) = pid.map(|p| -(p as i32)) {
        // SAFETY: probing, then signalling, the group of our own child.
        if unsafe { libc::kill(group, 0) } == 0 {
            unsafe { libc::kill(group, libc::SIGKILL) };
        }
    }
}
/// A dropped `AqGit` (the slot's task was cancelled) takes its whole process group with it, so
/// no engine survives holding the owner lock; `kill_on_drop` alone signals only the leader.
impl Drop for AqGit {
    fn drop(&mut self) {
        if self.child.is_some() {
            kill_group(self.leader);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
