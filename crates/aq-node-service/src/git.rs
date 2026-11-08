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
pub(crate) mod tests {
    use super::*;
    use tokio::net::UnixListener;
    fn repo(identity: &str) -> RepositoryBinding {
        RepositoryBinding {
            repository_id: Uuid::from_u128(9),
            identity: identity.into(),
            binding_generation: 1,
            object_format: "sha1".into(),
        }
    }
    fn binding(identity: &str, mode: &str) -> serde_json::Value {
        serde_json::json!({"identity": identity, "mode": mode, "binding_generation": 1,
            "object_format": "sha1", "repository_id": Uuid::from_u128(9)})
    }
    /// A fake aq-git socket: answers each connection's frame with `answer(request)`.
    fn serve(
        socket: &Path,
        answers: Vec<fn(&serde_json::Value) -> serde_json::Value>,
    ) -> tokio::task::JoinHandle<Vec<serde_json::Value>> {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            for answer in answers {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let reply = answer(&request);
                stream
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                seen.push(request);
            }
            seen
        })
    }
    fn ok(r: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"ok": true, "request_id": r["request_id"], "result": result})
    }
    fn refused(r: &serde_json::Value, code: &str, message: &str) -> serde_json::Value {
        serde_json::json!({"ok": false, "request_id": r["request_id"],
            "error": {"code": code, "message": message, "details": null}})
    }
    const ALREADY: &str = "Repository already registered";
    #[tokio::test]
    async fn register_sends_repo_register_frames() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("git.sock");
        let server = serve(
            &socket,
            vec![|r| ok(r, binding("e0-development", "worker")), |r| {
                ok(r, binding("e0-development", "other"))
            }],
        );
        let git = AqGit::over_socket(socket);
        let repo = repo("e0-development");
        git.register(std::slice::from_ref(&repo)).await.unwrap();
        let refused = git.register(&[repo]).await.unwrap_err();
        assert!(refused.to_string().contains("mismatch"), "{refused:#}");
        let seen = server.await.unwrap();
        assert_eq!(seen[0]["operation"], "repo.register");
        assert_eq!(seen[0]["expected_binding_generation"], 0);
        assert_eq!(seen[0]["params"]["binding"]["mode"], "worker");
        assert_eq!(seen[0]["params"]["binding"]["identity"], "e0-development");
        git.stop().await.unwrap();
    }
    /// D2 (a): recovery restarts aq-git on the attempt's state, where the repository is
    /// registered: the existing registration is read and accepted when it is this binding.
    #[tokio::test]
    async fn register_accepts_the_existing_registration_of_this_binding() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("git.sock");
        let server = serve(
            &socket,
            vec![
                |r| refused(r, "binding_conflict", ALREADY),
                |r| ok(r, binding("e0", "worker")),
                |r| refused(r, "binding_conflict", ALREADY),
                |r| ok(r, binding("another", "worker")),
                |r| {
                    refused(
                        r,
                        "binding_conflict",
                        "Registration requires generation zero",
                    )
                },
            ],
        );
        let git = AqGit::over_socket(socket);
        git.register(&[repo("e0")]).await.unwrap();
        let e = git.register(&[repo("e0")]).await.unwrap_err();
        assert!(e.to_string().contains("worker binding mismatch"), "{e:#}");
        let e = git.register(&[repo("e0")]).await.unwrap_err();
        assert!(e.to_string().contains("generation zero"), "{e:#}");
        let seen = server.await.unwrap();
        assert_eq!(seen[1]["operation"], "repo.show");
        assert_eq!(seen[1]["repository_id"], Uuid::from_u128(9).to_string());
        assert_eq!(seen[1]["expected_binding_generation"], 1);
    }

    /// A stand-in aq-git: `serve` listens on `--listen` (python3), `recovery clear-fence`
    /// saves its request beside the script and answers ok; `AQ_FAKE_EXIT` exits at once.
    pub(crate) fn fake_aq_git(dir: &Path, exit: Option<u8>) -> AqGitConfig {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("aq-git");
        let body = match exit {
            Some(code) => format!("#!/bin/sh\nexit {code}\n"),
            None => r#"#!/bin/sh
for a; do [ "$prev" = --listen ] && listen=$a; prev=$a; done
case " $* " in
*" clear-fence "*) cat > "$(dirname "$0")/clear-fence.json"; echo '{"ok":true,"result":{}}' ;;
*" serve "*) sleep 60 & echo $! > "$(dirname "$0")/engine.pid"
  exec python3 -c 'import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(8); time.sleep(60)' "$listen" ;;
esac
"#
            .to_owned(),
        };
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A thread forking elsewhere in the test process can briefly hold the write
        // descriptor (ETXTBSY on exec): wait until the script runs.
        for _ in 0..200 {
            match std::process::Command::new(&script).arg("--probe").status() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break,
            }
        }
        AqGitConfig {
            binary: script,
            git_executable: "/usr/bin/git".into(),
        }
    }
    /// The state of an aq-git that was SIGKILLed: a socket nobody listens on and an active
    /// incarnation whose owner lock is free.
    fn killed(state: &Path, socket: &Path) {
        std::fs::create_dir_all(state.join("git")).unwrap();
        std::fs::write(state.join("git/owner.lock"), b"").unwrap();
        let record = serde_json::json!({"active": true, "id": "inc-1", "guard_pid": 1,
            "boot_id": "b", "version": 1});
        std::fs::write(state.join("git/incarnation.json"), record.to_string()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
    }
    /// D2 (b): after a SIGKILL the stale socket is removed and the fence cleared (the owner
    /// lock is free), and start waits for the new aq-git to accept a connection.
    #[tokio::test]
    async fn start_reclaims_the_socket_and_fence_of_a_killed_aq_git() {
        let dir = tempfile::tempdir().unwrap();
        let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
        killed(&state, &socket);
        let config = fake_aq_git(dir.path(), None);
        let git = AqGit::start(&config, &state, &socket).await.unwrap();
        UnixStream::connect(&socket)
            .await
            .expect("the new aq-git listens");
        let request: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("clear-fence.json")).unwrap())
                .unwrap();
        assert_eq!(request["operation"], "recovery.clear-fence");
        assert_eq!(request["params"]["expected_incarnation"], "inc-1");
        assert_eq!(request["params"]["old_environment_stopped"], true);
        assert_eq!(request["params"]["evidence"]["owner_lock"], "free");
        git.stop().await.unwrap();
    }
    /// Nothing is removed while a process holds the owner lock or listens on the socket, and
    /// an aq-git that exits before it listens is reported at once.
    #[tokio::test]
    async fn start_keeps_what_a_live_owner_holds_and_reports_an_early_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
        killed(&state, &socket);
        let config = fake_aq_git(dir.path(), None);
        let held = std::fs::File::open(state.join("git/owner.lock")).unwrap();
        // SAFETY: flock on our own descriptor.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let e = format!(
            "{:#}",
            AqGit::start(&config, &state, &socket).await.err().unwrap()
        );
        assert!(e.contains("still owned by a live process"), "{e}");
        assert!(socket.exists());
        drop(held);
        std::fs::remove_file(&socket).unwrap();
        let live = UnixListener::bind(&socket).unwrap();
        let e = format!(
            "{:#}",
            AqGit::start(&config, &state, &socket).await.err().unwrap()
        );
        assert!(e.contains("a live aq-git still listens"), "{e}");
        drop(live);
        assert!(
            !dir.path().join("clear-fence.json").exists(),
            "the fence is kept"
        );
        std::fs::remove_file(state.join("git/incarnation.json")).unwrap();
        let exits = fake_aq_git(dir.path(), Some(3));
        let e = format!(
            "{:#}",
            AqGit::start(&exits, &state, &socket).await.err().unwrap()
        );
        assert!(
            e.contains("exited (exit status: 3) before it listened"),
            "{e}"
        );
    }
    /// D2 against the real aq-git: register, SIGKILL its process group (as `systemctl kill`
    /// does), start again on the same state and register again.
    #[tokio::test]
    #[ignore = "requires AQ_GIT_TEST_BINARY (an aq-git build)"]
    async fn real_aq_git_restarts_after_a_sigkill() {
        let binary = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set AQ_GIT_TEST_BINARY");
        let dir = tempfile::tempdir().unwrap();
        let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
        std::fs::create_dir_all(state.join("git")).unwrap();
        let config = AqGitConfig {
            binary: binary.into(),
            git_executable: "/usr/bin/git".into(),
        };
        let repo = repo("e0-development");
        let mut git = AqGit::start(&config, &state, &socket).await.unwrap();
        git.register(std::slice::from_ref(&repo)).await.unwrap();
        let pgid = git.child.as_ref().unwrap().id().unwrap() as i32;
        // SAFETY: the test's own aq-git process group.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        git.child.take().unwrap().wait().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(socket.exists(), "the killed aq-git left its socket");
        let git = AqGit::start(&config, &state, &socket).await.unwrap();
        git.register(std::slice::from_ref(&repo)).await.unwrap();
        git.stop().await.unwrap();
        let git = AqGit::start(&config, &state, &socket).await.unwrap();
        git.register(&[repo]).await.unwrap();
        git.stop().await.unwrap();
    }
    /// #51 review M3: a stopped or dropped aq-git takes its whole process group (the forked
    /// engine holds the owner lock), not only its leader.
    #[tokio::test]
    async fn stop_and_drop_kill_the_whole_process_group() {
        for drop_it in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
            std::fs::create_dir_all(state.join("git")).unwrap();
            let config = fake_aq_git(dir.path(), None);
            let git = AqGit::start(&config, &state, &socket).await.unwrap();
            let engine: i32 = std::fs::read_to_string(dir.path().join("engine.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // SAFETY: probing a process the test started.
            assert_eq!(unsafe { libc::kill(engine, 0) }, 0, "the engine runs");
            if drop_it {
                drop(git);
            } else {
                git.stop().await.unwrap();
            }
            let gone = (0..200).any(|_| {
                std::thread::sleep(Duration::from_millis(10));
                // A killed child of the shell is a zombie until reaped: check its state.
                std::fs::read_to_string(format!("/proc/{engine}/stat")).map_or(true, |s| {
                    s.rsplit_once(") ").is_some_and(|(_, t)| t.starts_with('Z'))
                })
            });
            assert!(
                gone,
                "the engine of a {} aq-git is gone",
                if drop_it { "dropped" } else { "stopped" }
            );
        }
    }
    /// #51 re-review: the group kill uses the leader pid taken at spawn, so it still fires
    /// when the leader exited and was reaped before the stop (its engine still running).
    #[tokio::test]
    async fn the_group_kill_fires_after_the_leader_was_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
        std::fs::create_dir_all(state.join("git")).unwrap();
        let config = fake_aq_git(dir.path(), None);
        let mut git = AqGit::start(&config, &state, &socket).await.unwrap();
        let engine: i32 = std::fs::read_to_string(dir.path().join("engine.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The leader exits on its own and is reaped: its `Child::id` is gone.
        let child = git.child.as_mut().unwrap();
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        assert!(child.id().is_none());
        git.stop().await.unwrap();
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            std::fs::read_to_string(format!("/proc/{engine}/stat")).map_or(true, |s| {
                s.rsplit_once(") ").is_some_and(|(_, t)| t.starts_with('Z'))
            })
        });
        assert!(gone, "the engine is gone after the stop");
    }
}
