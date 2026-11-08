//! The worker-side aq-git child, a port of the exchange/stop steps of aq-server's
//! `scripts/ms1-node-run.py`.
use crate::config::{AqGitConfig, RepositoryBinding};
use anyhow::{Context, Result, bail, ensure};
use std::{
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
    socket: PathBuf,
}
impl AqGit {
    /// Spawns `<binary> --state-dir <dir>/git --listen <dir>/git.sock --git-executable <git> serve`
    /// (stdio null, own process group) and waits up to 10 s for the socket.
    pub async fn start(config: &AqGitConfig, state_dir: &Path) -> Result<Self> {
        let socket = state_dir.join("git.sock");
        let mut child = Command::new(&config.binary)
            .arg("--state-dir")
            .arg(state_dir.join("git"))
            .arg("--listen")
            .arg(&socket)
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
        while !socket.exists() {
            if child.try_wait()?.is_some() || tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                bail!("worker-side aq-git did not listen");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self {
            child: Some(child),
            socket,
        })
    }
    #[cfg(test)]
    pub(crate) fn over_socket(socket: PathBuf) -> Self {
        Self {
            child: None,
            socket,
        }
    }
    /// repo.register for each binding (expected_binding_generation 0); refuses unless the
    /// reply has ok:true, the same request_id and mode "worker".
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
            ensure!(
                reply["ok"] == true && reply["request_id"] == id.as_str(),
                "aq-git refused repo.register: {}",
                reply["error"]
                    .to_string()
                    .chars()
                    .take(500)
                    .collect::<String>()
            );
            ensure!(
                reply["result"]["mode"] == "worker",
                "worker binding mismatch"
            );
        }
        Ok(())
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
    /// SIGTERM, wait 10 s, then SIGKILL.
    pub async fn stop(mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Some(pid) = child.id() {
            // SAFETY: plain signal to our own child.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_err()
        {
            child.kill().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;
    #[tokio::test]
    async fn register_sends_repo_register_frames() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("git.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for mode in ["worker", "other"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let reply = serde_json::json!({"ok": true, "request_id": request["request_id"], "result": {"mode": mode}});
                stream
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                seen.push(request);
            }
            seen
        });
        let git = AqGit::over_socket(socket);
        let repo = RepositoryBinding {
            repository_id: Uuid::from_u128(9),
            identity: "e0-development".into(),
            binding_generation: 1,
            object_format: "sha1".into(),
        };
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
}
