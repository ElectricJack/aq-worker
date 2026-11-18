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

pub(super) async fn run(
    root: PathBuf,
    node: Uuid,
    link: ServerLink,
    cfg: DockerLocalConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut warned = std::collections::HashSet::new();
    while !*shutdown.borrow() {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let Ok(bytes) = std::fs::read(dir.join("state.json")) else {
                    continue;
                };
                let Ok(agent) = serde_json::from_slice::<Agent>(&bytes) else {
                    continue;
                };
                if agent.node_id != node
                    || agent.status != "stopped"
                    || dir.file_name().and_then(|s| s.to_str()) != Some(&agent.id.to_string())
                {
                    continue;
                }
                tokio::select! {
                    result = archive(&dir, &agent, &link, &cfg) => {
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
