//! Persistent, owner-started tmux sessions in the fixed local sandbox.
mod provider;
use anyhow::{Context, Result, bail, ensure};
use aq_node_service::{
    config::ServiceConfig,
    enrollment_file::{EnrollmentFile, write_private},
    layout::Layout,
    link::ServerLink,
};
use aq_worker::{
    backend::docker_local::{
        DockerLocalBackend, DockerLocalConfig,
        tar::{TreeEntry, tree_archive},
    },
    backend::{
        CreateRequest, Ownership, PlacementBackend, PrivateMaterial, ResourceLimits,
        SandboxIdentity, SandboxState,
    },
    native::runner::ClaudeDockerSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
};
use uuid::Uuid;
const SOCKET: &str = "/aq/out/manual.sock";
const RUNNER: &str = r#"set -euo pipefail
umask 077
mkdir -p /aq/out /aq/work
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
stage(){ printf '%s' "$1" > /aq/out/stage; }
trap 'tmux -S /aq/out/manual.sock kill-server 2>/dev/null || true; exit 0' TERM INT HUP
parent=$$
(while sleep 2; do until=$(cat /aq/out/lease 2>/dev/null || echo 0); if [ "$(date +%s)" -ge "$until" ]; then printf 'Worker connection expired; workspace retained' > /aq/out/stage; kill -TERM "$parent"; exit; fi; done) &
watchdog=$!
stage 'Fetching project source'
cd /aq/work
git -c core.hooksPath=/dev/null init -q
repo=$(cat /aq/in/repository); ref=$(cat /aq/in/source)
git remote add origin "$repo"
while ! timeout 40 git -c core.hooksPath=/dev/null -c http.lowSpeedLimit=1 -c http.lowSpeedTime=25 fetch --no-tags origin "$ref"; do stage 'Retrying source download'; sleep 2; done
stage 'Checking out project source'
git -c core.hooksPath=/dev/null checkout --detach FETCH_HEAD
stage 'Starting interactive agent'
tmux -S /aq/out/manual.sock new-session -d -s aq -x 120 -y 40 '/bin/bash /aq/in/agent.sh'
stage 'Ready to connect'
while tmux -S /aq/out/manual.sock has-session -t aq 2>/dev/null; do sleep 1; done
stage 'Agent exited; workspace retained'
kill "$watchdog" 2>/dev/null || true
"#;
#[derive(Clone, Serialize, Deserialize)]
struct Agent {
    id: Uuid,
    node_id: Uuid,
    project_id: Uuid,
    spec: Value,
    status: String,
    stage: String,
    desired_running: bool,
    #[serde(default)]
    sandbox: Option<SandboxIdentity>,
    #[serde(default)]
    acknowledged: Vec<Uuid>,
}
impl Agent {
    fn report(&self, snapshot: Option<String>) -> Value {
        json!({"id":self.id,"status":self.status,"stage":self.stage,"snapshot":snapshot,"acknowledged":self.acknowledged})
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn file(path: &str, data: impl Into<Vec<u8>>) -> TreeEntry {
    TreeEntry::File {
        path: path.into(),
        mode: 0o600,
        data: data.into(),
    }
}
fn save(root: &Path, a: &Agent) -> Result<()> {
    write_private(
        &root.join(a.id.to_string()).join("state.json"),
        &serde_json::to_vec(a)?,
    )
}
async fn cli(exe: &Path, socket: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut cmd = Command::new(exe);
    cmd.args(["--remote", "--url"])
        .arg(format!("unix://{}", socket.display()))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("start sandbox terminal client")?;
    if let Some(bytes) = input {
        child
            .stdin
            .take()
            .context("terminal client stdin")?
            .write_all(bytes)
            .await?;
    }
    let output = async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("terminal client stdout")?
            .take(262145)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 262144, "terminal output exceeded bound");
        ensure!(
            child.wait().await?.success(),
            "sandbox terminal client refused operation"
        );
        Ok::<_, anyhow::Error>(bytes)
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), output)
        .await
        .context("sandbox terminal client timed out")?
}
async fn tmux(
    exe: &Path,
    cfg: &DockerLocalConfig,
    container: &str,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut v = vec!["exec"];
    if input.is_some() {
        v.push("-i");
    }
    v.extend([container, "tmux", "-S", SOCKET]);
    v.extend(args);
    cli(exe, &cfg.socket, &v, input).await
}
fn source(spec: &Value) -> Result<String> {
    let s = &spec["source"];
    if s["kind"] == "immutable" {
        let hex = s["object"]["hex"].as_str().context("immutable source")?;
        ensure!(
            hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source object"
        );
        return Ok(hex.into());
    }
    let r = s["reference"]
        .as_str()
        .or_else(|| s["ref"].as_str())
        .context("source reference is unavailable")?;
    ensure!(
        r.starts_with("refs/heads/") && !r.contains(['\n', '\0', ' ', ':']) && !r.contains(".."),
        "invalid source ref"
    );
    Ok(r.into())
}
async fn create(
    config: &ServiceConfig,
    settings: &ClaudeDockerSettings,
    node: Uuid,
    a: &mut Agent,
    root: &Path,
) -> Result<()> {
    let repo = a.spec["repository_id"]
        .as_str()
        .context("repository identity")?;
    let repo = config
        .repositories
        .iter()
        .find(|r| r.repository_id.to_string() == repo)
        .context("project repository is not installed on this worker")?;
    let slug = config
        .manual
        .as_ref()
        .and_then(|m| m.source_slugs.get(&repo.repository_id))
        .unwrap_or(&repo.identity);
    ensure!(valid_slug(slug), "repository source is not configured");
    let plan = provider::plan(config, settings, &a.spec)?;
    let mut docker = settings.host.docker_local.clone();
    docker.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), RUNNER.into()]);
    let mut backend = DockerLocalBackend::connect_unix(docker).await?;
    let request = CreateRequest::new(
        Ownership {
            node_id: node,
            boot_id: a.id,
            attempt_id: a.id,
        },
        settings.host.template.clone(),
        u64::MAX,
        vec![],
    )
    .with_resources(ResourceLimits {
        cpu_millis: 2000,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        pids: 512,
        disk_bytes: None,
    });
    let sandbox = backend.create(&request).await?;
    a.sandbox = Some(sandbox.clone());
    save(root, a)?;
    let context = format!(
        "You are an interactive coding agent for {} in an isolated workspace. Wait for the owner's instructions. Read AGENTS.md and .aq/PRIME.md before doing project work. Keep build outputs outside the retained source tree. Do not change other projects, host services, credentials or pools. This session stays open between requests; the owner stops it explicitly.\n",
        a.spec["project_name"].as_str().unwrap_or("this project")
    );
    let mut entries = vec![
        file("repository", format!("https://github.com/{slug}.git")),
        file("source", source(&a.spec)?),
        file("context", context),
    ];
    entries.extend(plan.entries);
    backend
        .put_archive(&sandbox, tree_archive("aq/in", &entries)?)
        .await?;
    backend
        .put_archive(
            &sandbox,
            tree_archive(
                "aq/out",
                &[file("lease", ((now() + 45000) / 1000).to_string())],
            )?,
        )
        .await?;
    let bytes = aq_handoff_note::read_private(&plan.secret, 65536)?;
    provider::validate_material(&a.spec, &bytes)?;
    backend
        .deliver_private_material(&sandbox, PrivateMaterial::new(bytes.to_vec()))
        .await?;
    backend.start_runner(&sandbox).await?;
    a.stage = "Fetching project source".into();
    save(root, a)?;
    Ok(())
}
async fn observe(
    exe: &Path,
    cfg: &DockerLocalConfig,
    a: &mut Agent,
    lease_until: u64,
) -> Result<Option<String>> {
    let sandbox = a.sandbox.as_ref().context("manual sandbox missing")?;
    let mut backend = DockerLocalBackend::connect_unix(cfg.clone()).await?;
    let state = backend.inspect(sandbox).await?;
    if state.state == SandboxState::Stopped {
        a.status = "stopped".into();
        a.stage = "Agent stopped; workspace retained".into();
        return Ok(None);
    }
    cli(
        exe,
        &cfg.socket,
        &[
            "exec",
            "-i",
            &sandbox.backend_id,
            "/bin/sh",
            "-c",
            "umask 077; cat > /aq/out/lease",
        ],
        Some((lease_until / 1000).to_string().as_bytes()),
    )
    .await?;
    if tmux(
        exe,
        cfg,
        &sandbox.backend_id,
        &["has-session", "-t", "aq"],
        None,
    )
    .await
    .is_ok()
    {
        a.status = "running".into();
        a.stage = "Ready to connect".into();
        let bytes = tmux(
            exe,
            cfg,
            &sandbox.backend_id,
            &["capture-pane", "-p", "-e", "-t", "aq", "-S", "-500"],
            None,
        )
        .await?;
        let mut text = if aq_agent_channel::holds_agent_credential(&bytes)
            || aq_agent_channel::holds_claude_token_shape(&bytes)
        {
            "[Output hidden because it contained credentials]".to_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        if text.len() > 131072 {
            let mut start = text.len() - 131072;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            text = text[start..].into();
        }
        return Ok(Some(text));
    }
    if let Ok(bytes) = cli(
        exe,
        &cfg.socket,
        &["exec", &sandbox.backend_id, "cat", "/aq/out/stage"],
        None,
    )
    .await
    {
        let stage = String::from_utf8_lossy(&bytes);
        a.stage = stage.chars().take(128).collect();
    }
    Ok(None)
}
async fn apply(
    exe: &Path,
    cfg: &DockerLocalConfig,
    a: &mut Agent,
    commands: &[Value],
    root: &Path,
) -> Result<()> {
    let container = a
        .sandbox
        .as_ref()
        .context("manual sandbox absent")?
        .backend_id
        .clone();
    for c in commands {
        let id: Uuid = serde_json::from_value(c["operation_id"].clone())?;
        if a.acknowledged.contains(&id) {
            continue;
        }
        // Record uncertainty BEFORE input. A restart must never replay these bytes.
        let marker = root.join(a.id.to_string()).join(format!("input-{id}"));
        if marker.exists() {
            continue;
        }
        write_private(&marker, b"dispatched")?;
        if let Some(bytes) = c["payload"]["bytes"].as_array() {
            let bytes = bytes
                .iter()
                .map(|b| {
                    b.as_u64()
                        .filter(|n| *n <= 255)
                        .map(|n| n as u8)
                        .context("input byte")
                })
                .collect::<Result<Vec<_>>>()?;
            ensure!(bytes.len() <= 4096, "input bound");
            if let Some(key) = terminal_key(&bytes) {
                // TUI Enter must be a key event, not pasted multiline text.
                // Only fixed key names enter argv; typed text stays on stdin.
                tmux(exe, cfg, &container, &["send-keys", "-t", "aq", key], None).await?;
            } else {
                let name = id.to_string();
                tmux(
                    exe,
                    cfg,
                    &container,
                    &["load-buffer", "-b", &name, "-"],
                    Some(&bytes),
                )
                .await?;
                tmux(
                    exe,
                    cfg,
                    &container,
                    &["paste-buffer", "-r", "-d", "-b", &name, "-t", "aq"],
                    None,
                )
                .await?;
            }
        } else {
            let columns = c["payload"]["columns"].as_u64().context("columns")?;
            let rows = c["payload"]["rows"].as_u64().context("rows")?;
            ensure!(
                (2..=500).contains(&columns) && (1..=300).contains(&rows),
                "resize bound"
            );
            tmux(
                exe,
                cfg,
                &container,
                &[
                    "resize-window",
                    "-t",
                    "aq",
                    "-x",
                    &columns.to_string(),
                    "-y",
                    &rows.to_string(),
                ],
                None,
            )
            .await?;
        }
        a.acknowledged.push(id);
        save(root, a)?;
    }
    Ok(())
}
struct Node {
    id: Uuid,
    link: ServerLink,
    agents: BTreeMap<Uuid, Agent>,
    snapshots: BTreeMap<Uuid, String>,
    lease_until: u64,
}
pub async fn run(
    config: ServiceConfig,
    layout: Layout,
    mut shutdown: watch::Receiver<bool>,
    paused: BTreeMap<Uuid, watch::Sender<bool>>,
    native_active: BTreeMap<Uuid, Arc<AtomicUsize>>,
) -> Result<()> {
    let Some(manual) = config.manual.as_ref() else {
        return Ok(());
    };
    let native: aq_worker::native::runner::NativeConfig =
        serde_json::from_slice(&tokio::fs::read(&config.native_template).await?)?;
    let Some(aq_worker::native::runner::ExecutorConfig::ClaudeDockerLocalResultLocal {
        settings,
        ..
    }) = native.executor
    else {
        bail!("manual sessions require the installed Claude sandbox")
    };
    let exe = manual.podman_executable.clone();
    let max_slots = manual.max_slots;
    let providers = provider::available(&config);
    let root = config.state_root.join("manual-agents");
    std::fs::create_dir_all(&root)?;
    let mut nodes = Vec::new();
    for slot in 1..=config.slots {
        let enrollment = EnrollmentFile::load(&layout.slot_enrollment(slot))?;
        let mut agents = BTreeMap::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path().join("state.json");
            if !path.exists() {
                continue;
            }
            if let Ok(a) = serde_json::from_slice::<Agent>(&std::fs::read(path)?)
                && a.node_id == enrollment.node_id
                && !matches!(a.status.as_str(), "stopped" | "failed")
            {
                agents.insert(a.id, a);
            }
        }
        nodes.push(Node {
            id: enrollment.node_id,
            link: ServerLink::from_enrollment(&enrollment)?,
            agents,
            snapshots: BTreeMap::new(),
            lease_until: 0,
        });
    }
    while !*shutdown.borrow() {
        for node in &mut nodes {
            let mut pending = false;
            let active = native_active
                .get(&node.id)
                .context("manual admission counter absent")?;
            let pause = paused
                .get(&node.id)
                .context("manual admission pause absent")?;
            let reports = node
                .agents
                .values()
                .map(|a| a.report(node.snapshots.get(&a.id).cloned()))
                .collect::<Vec<_>>();
            let request = json!({"providers":providers,"max_slots":max_slots,"repositories":supported_repositories(&config),"busy":active.load(Ordering::SeqCst)>0,"reports":reports});
            let response = match node.link.manual_poll(&request).await {
                Ok(v) => v,
                Err(_) => {
                    continue;
                }
            };
            node.lease_until = now() + response["lease_ms"].as_u64().unwrap_or(0).min(45000);
            pending |= response["pending_count"].as_u64().unwrap_or(0) > 0;
            // Stop future native admission before acting on manual claims; a currently
            // executing native task finishes normally and keeps its custody.
            if pending {
                pause.send_replace(true);
            }
            let jobs = response["sessions"]
                .as_array()
                .context("manual session response")?;
            let ids = jobs
                .iter()
                .filter_map(|v| v["id"].as_str().and_then(|id| id.parse::<Uuid>().ok()))
                .collect::<Vec<_>>();
            node.agents.retain(|id, a| {
                ids.contains(id) || !matches!(a.status.as_str(), "stopped" | "failed")
            });
            for a in node.agents.values_mut() {
                // A successful poll confirms that the server received these
                // acknowledgements. Keep only uncertain delivery markers.
                for id in a.acknowledged.drain(..) {
                    let _ = std::fs::remove_file(
                        root.join(a.id.to_string()).join(format!("input-{id}")),
                    );
                }
            }
            for job in jobs {
                let id: Uuid = serde_json::from_value(job["id"].clone())?;
                let a = node.agents.entry(id).or_insert_with(|| Agent {
                    id,
                    node_id: node.id,
                    project_id: serde_json::from_value(job["project_id"].clone())
                        .unwrap_or(Uuid::nil()),
                    spec: job["spec"].clone(),
                    status: "preparing".into(),
                    stage: "Waiting for automated work to release the worker".into(),
                    desired_running: job["desired_running"] == true,
                    sandbox: None,
                    acknowledged: Vec::new(),
                });
                a.desired_running = job["desired_running"] == true;
                if !a.desired_running {
                    if let Some(sandbox) = &a.sandbox {
                        if a.spec["provider"] == "openai" {
                            let _ = cli(
                                &exe,
                                &settings.host.docker_local.socket,
                                &[
                                    "exec",
                                    &sandbox.backend_id,
                                    "rm",
                                    "-f",
                                    "/aq/home/.codex/auth.json",
                                ],
                                None,
                            )
                            .await;
                        }
                        let mut backend =
                            DockerLocalBackend::connect_unix(settings.host.docker_local.clone())
                                .await?;
                        if !backend.stop(sandbox).await.is_confirmed() {
                            a.status = "stopping".into();
                            a.stage = "Waiting for sandbox stop confirmation".into();
                            save(&root, a)?;
                            continue;
                        }
                    }
                    a.status = "stopped".into();
                    a.stage = "Stopped; workspace retained".into();
                    save(&root, a)?;
                    continue;
                }
                if a.sandbox.is_none() {
                    if active.load(Ordering::SeqCst) > 0 {
                        continue;
                    }
                    if !memory_available() {
                        a.stage = "Waiting for memory to become available".into();
                        save(&root, a)?;
                        continue;
                    }
                    if let Err(_error) = create(&config, &settings, node.id, a, &root).await {
                        a.status = "failed".into();
                        a.stage = "Startup failed; workspace retained for diagnosis".into();
                        save(&root, a)?;
                        continue;
                    }
                }
                let commands = response["commands"][id.to_string()]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if apply(&exe, &settings.host.docker_local, a, &commands, &root)
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id=%id,"manual terminal input is uncertain; bytes will not be replayed");
                }
                match observe(&exe, &settings.host.docker_local, a, node.lease_until).await {
                    Ok(Some(text)) => {
                        node.snapshots.insert(id, text);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        a.stage = "Reconnecting to the same terminal".into();
                    }
                }
                save(&root, a)?;
            }
            pause.send_replace(pending);
        }
        tokio::select! {_=tokio::time::sleep(std::time::Duration::from_millis(500))=>{},_=shutdown.changed()=>{}}
    }
    for node in &mut nodes {
        for a in node.agents.values_mut() {
            if let Some(sandbox) = &a.sandbox {
                let mut backend =
                    DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
                if backend.stop(sandbox).await.is_confirmed() {
                    a.status = "stopped".into();
                    a.stage = "Worker stopped; workspace retained".into();
                    save(&root, a)?;
                }
            }
        }
        let _=node.link.manual_poll(&json!({"providers":providers,"max_slots":max_slots,"repositories":supported_repositories(&config),"busy":true,"reports":node.agents.values().map(|a|a.report(None)).collect::<Vec<_>>()})).await;
    }
    for pause in paused.values() {
        pause.send_replace(false);
    }
    Ok(())
}

fn terminal_key(bytes: &[u8]) -> Option<&'static str> {
    Some(match bytes {
        b"\r" => "Enter",
        b"\t" => "Tab",
        b"\x1b" => "Escape",
        b"\x7f" | b"\x08" => "BSpace",
        b"\x03" => "C-c",
        b"\x04" => "C-d",
        b"\x1a" => "C-z",
        b"\x1b[A" | b"\x1bOA" => "Up",
        b"\x1b[B" | b"\x1bOB" => "Down",
        b"\x1b[C" | b"\x1bOC" => "Right",
        b"\x1b[D" | b"\x1bOD" => "Left",
        _ => return None,
    })
}
fn valid_slug(slug: &str) -> bool {
    slug.split('/').count() == 2
        && slug
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
}
fn supported_repositories(config: &ServiceConfig) -> Vec<Uuid> {
    config
        .repositories
        .iter()
        .filter(|r| {
            valid_slug(
                config
                    .manual
                    .as_ref()
                    .and_then(|m| m.source_slugs.get(&r.repository_id))
                    .unwrap_or(&r.identity),
            )
        })
        .map(|r| r.repository_id)
        .collect()
}
fn memory_available() -> bool {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("MemAvailable:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .is_some_and(|kb| kb >= 5 * 1024 * 1024)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_objects_and_repository_slugs_are_closed_and_literal() {
        assert_eq!(
            source(&json!({"source":{"kind":"immutable","object":{"hex":"a".repeat(40)}}}))
                .unwrap(),
            "a".repeat(40)
        );
        assert!(
            source(&json!({"source":{"kind":"immutable","object":{"hex":"--upload-pack=bad"}}}))
                .is_err()
        );
        assert!(
            source(&json!({"source":{"kind":"named_ref","reference":"refs/heads/main:evil"}}))
                .is_err()
        );
        assert!(valid_slug("ElectricJack/aq-server"));
        assert!(!valid_slug("../aq-server"));
        assert!(!valid_slug("https://evil/repo"));
        assert!(!valid_slug("owner/repo\ncommand"));
    }
}
