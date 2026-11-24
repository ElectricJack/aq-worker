//! Save native user and assistant messages from stopped manual sandboxes.
use super::Agent;
use aq_node_service::{enrollment_file::write_private, link::ServerLink};
use aq_worker::{
    backend::docker_local::{DockerLocalBackend, DockerLocalConfig},
    conversation::{Provider, read_snapshot},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

async fn read_agent_snapshot(
    agent: &Agent,
    cfg: &DockerLocalConfig,
) -> anyhow::Result<Option<aq_worker::conversation::Snapshot>> {
    let provider = match agent.spec["provider"].as_str() {
        Some("anthropic") => Provider::Claude,
        Some("openai") => Provider::Codex,
        _ => return Ok(None),
    };
    let Some(sandbox) = &agent.sandbox else {
        return Ok(None);
    };
    let mut backend = DockerLocalBackend::connect_unix(cfg.clone()).await?;
    read_snapshot(
        &mut backend,
        sandbox,
        provider,
        provider.log_directory(),
        None,
        None,
    )
    .await
}

async fn archive(
    directory: &Path,
    agent: &Agent,
    link: &ServerLink,
    cfg: &DockerLocalConfig,
) -> anyhow::Result<()> {
    let done = directory.join("conversation.done");
    if done.exists() {
        return Ok(());
    }
    let pending = directory.join("conversation.pending.json");
    let body = match std::fs::read(&pending) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(snapshot) = read_agent_snapshot(agent, cfg).await? else {
                return write_private(&done, b"no native conversation");
            };
            let body = serde_json::to_vec(&snapshot)?;
            write_private(&pending, &body)?;
            body
        }
        Err(error) => return Err(error.into()),
    };
    let receipt = link
        .manual_conversation(agent.id, &serde_json::from_slice(&body)?)
        .await?;
    anyhow::ensure!(
        receipt["session_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            == Some(agent.id),
        "conversation acknowledgement did not match session"
    );
    write_private(&done, b"archived")?;
    std::fs::remove_file(pending)?;
    Ok(())
}

pub(super) struct Archiver {
    pub root: PathBuf,
    pub node: Uuid,
    pub link: ServerLink,
    pub cfg: DockerLocalConfig,
}

impl Archiver {
    fn stopped_agent(&self, directory: &Path) -> Option<Agent> {
        let bytes = std::fs::read(directory.join("state.json")).ok()?;
        let agent: Agent = serde_json::from_slice(&bytes).ok()?;
        if agent.node_id != self.node || agent.status != "stopped" {
            return None;
        }
        let directory_id = directory.file_name()?.to_str()?;
        (directory_id == agent.id.to_string()).then_some(agent)
    }
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut warned = std::collections::HashSet::new();
        while !*shutdown.borrow() {
            if let Ok(entries) = std::fs::read_dir(&self.root) {
                for entry in entries.flatten() {
                    let dir = entry.path();
                    let Some(agent) = self.stopped_agent(&dir) else {
                        continue;
                    };
                    tokio::select! {
                        result = archive(&dir, &agent, &self.link, &self.cfg) => {
                            if result.is_err() && warned.insert(agent.id) {
                                tracing::warn!(session_id = %agent.id, "native conversation archive will be retried");
                            }
                        }
                        _ = shutdown.changed() => return,
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => {},
                _ = shutdown.changed() => return,
            }
        }
    }
}
