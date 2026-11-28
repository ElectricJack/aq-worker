//! Persistent, owner-started tmux sessions in the fixed local sandbox.
mod conversation;
mod provider;
use anyhow::{Context, Result, bail, ensure};
use aq_agent_channel::login::LoginLease;
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
use zeroize::Zeroizing;
const SOCKET: &str = "/aq/out/manual.sock";
const RUNNER: &str = r#"set -euo pipefail
umask 077
mkdir -p /aq/out /aq/work
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
stage(){ printf '%s' "$1" > /aq/out/stage; }
trap 'tmux -S /aq/out/manual.sock kill-server 2>/dev/null || true; rm -f /aq/home/.codex/auth.json; exit 0' TERM INT HUP
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
tmux -S /aq/out/manual.sock new-session -d -s aq -x 120 -y 40 'exec /bin/bash /aq/in/agent.sh'
stage 'Ready to connect'
while tmux -S /aq/out/manual.sock has-session -t aq 2>/dev/null; do sleep 1; done
stage 'Agent exited; workspace retained'
# The worker writes a login Codex refreshed back to the server, then removes it. The watchdog bounds the wait.
while [ -e /aq/home/.codex/auth.json ]; do sleep 1; done
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
    /// The public half of the Codex login leased for this session: its stored version
    /// and what identifies a refresh. No token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<LoginLease>,
    #[serde(skip)]
    login_checked: Option<std::time::Instant>,
}
/// The server refused to lease the session's login; its reason is the session's stage.
#[derive(Debug)]
struct LoginRefused(String);
impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for LoginRefused {}
const CODEX_AUTH: &str = "/aq/home/.codex/auth.json";
// Under Codex's own write lock (an exclusive flock on `auth.lock`, which Codex holds across
// its read, refresh and write: codex-rs `login/src/auth/storage.rs` `lock_for_write`), so a
// refresh in flight cannot write over the installed login. Replaced by rename: Codex reads
// the file by path and never sees a part of it.
const INSTALL_AUTH: &str = "umask 077; d=/aq/home/.codex; exec 9>>\"$d/auth.lock\" && flock 9 && cat > \"$d/auth.json.aq\" && mv -f \"$d/auth.json.aq\" \"$d/auth.json\"";
/// Keeps a Codex session's login and the server's login store (the one source of truth)
/// in step. A login Codex refreshed in the sandbox (the same account, refreshed later) is
/// written back as the next stored version. A newer stored version (ours was superseded,
/// or another copy refreshed and so ended the token ours holds) is installed over the
/// sandbox's `auth.json`: Codex re-reads that file before it refreshes and after a 401
/// (codex-rs `login/src/auth/manager.rs`, 0.166), so the running agent keeps working.
/// `install` is false at stop, when only the write-back matters.
async fn sync_login(
    exe: &Path,
    cfg: &DockerLocalConfig,
    link: &ServerLink,
    a: &mut Agent,
    install: bool,
) -> Result<()> {
    let (Some(lease), Some(sandbox)) = (a.login.clone(), a.sandbox.as_ref()) else {
        return Ok(());
    };
    let container = sandbox.backend_id.clone();
    let valid = aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json;
    let read = ["exec", &container, "cat", CODEX_AUTH];
    let current = Zeroizing::new(cli(exe, &cfg.socket, &read, None).await?);
    let mut superseded = false;
    if valid(&current).is_ok() && lease.refreshed(&current) {
        match link
            .session_login_refresh(a.id, lease.base_version, &current)
            .await?
        {
            (true, version) => {
                a.login = Some(LoginLease::of_session(&current, a.id, version)?);
                return Ok(());
            }
            (false, _) => superseded = true,
        }
    }
    if !install {
        return Ok(());
    }
    let (newest, version) = link.session_login(a.id).await?;
    if superseded || version > lease.base_version {
        valid(&newest)?;
        let write = ["exec", "-i", &container, "/bin/sh", "-c", INSTALL_AUTH];
        cli(exe, &cfg.socket, &write, Some(&newest)).await?;
        a.login = Some(LoginLease::of_session(&newest, a.id, version)?);
    }
    Ok(())
}
/// Before a Codex session's sandbox stops: the last write-back, then the login leaves
/// the retained workspace.
async fn finish_login(exe: &Path, cfg: &DockerLocalConfig, link: &ServerLink, a: &mut Agent) {
    if a.spec["provider"] != "openai" {
        return;
    }
    if let Err(error) = sync_login(exe, cfg, link, a, false).await {
        tracing::warn!(session_id=%a.id, %error, "Codex login write-back at stop");
    }
    if let Some(sandbox) = &a.sandbox {
        let remove = ["exec", &sandbox.backend_id, "rm", "-f", CODEX_AUTH];
        let _ = cli(exe, &cfg.socket, &remove, None).await;
    }
    // Written back once: a repeated stop has nothing left to compare.
    a.login = None;
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
    link: &ServerLink,
    a: &mut Agent,
    root: &Path,
) -> Result<()> {
    // The server names the repository it assigned: no list of repositories lives here.
    let slug = a.spec["repository_identity"]
        .as_str()
        .context("the server's session spec names no repository_identity")?;
    ensure!(
        valid_slug(slug),
        "the session's repository is not an owner/name"
    );
    let plan = provider::plan(config, settings, &a.spec)?;
    // Before any sandbox exists: a refused lease leaves nothing to retain.
    let bytes = match &plan.secret {
        Some(file) => aq_handoff_note::read_private(file, 65536)?,
        None => {
            let leased = link.session_login(a.id).await;
            let (bytes, version) = leased.map_err(|e| LoginRefused(e.to_string()))?;
            a.login = Some(LoginLease::of_session(&bytes, a.id, version)?);
            bytes
        }
    };
    provider::validate_material(&a.spec, &bytes)?;
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
    link: &ServerLink,
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
        if stage.starts_with("Agent exited;") {
            // The runner keeps the sandbox up until the login is collected.
            finish_login(exe, cfg, link, a).await;
            let sandbox = a.sandbox.as_ref().context("manual sandbox missing")?;
            if backend.stop(sandbox).await.is_confirmed() {
                a.status = "stopped".into();
                a.stage = "Agent stopped; workspace retained".into();
            }
        } else {
            a.stage = stage.chars().take(128).collect();
        }
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
impl Agent {
    fn needs_recovery(&self, node_id: Uuid) -> bool {
        let finished =
            self.status == "stopped" || (self.status == "failed" && self.sandbox.is_none());
        self.node_id == node_id && !finished
    }

    fn login_check_due(&self, period: std::time::Duration) -> bool {
        self.status == "running"
            && self.login.is_some()
            && self
                .login_checked
                .is_none_or(|checked| checked.elapsed() >= period)
    }
}

fn recover_agents(root: &Path, node_id: Uuid) -> Result<BTreeMap<Uuid, Agent>> {
    let mut agents = BTreeMap::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path().join("state.json");
        if !path.exists() {
            continue;
        }
        if let Ok(a) = serde_json::from_slice::<Agent>(&std::fs::read(path)?)
            && a.needs_recovery(node_id)
        {
            agents.insert(a.id, a);
        }
    }
    Ok(agents)
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
    sessions: BTreeMap<Uuid, Arc<aq_node_service::session::NodeSession>>,
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
    let legacy_max_slots = manual.max_slots;
    let providers = provider::available(&config);
    let login_check =
        std::time::Duration::from_secs((manual.codex.as_ref()).map_or(60, |c| c.login_check_secs));
    let root = config.state_root.join("manual-agents");
    std::fs::create_dir_all(&root)?;
    let mut nodes = Vec::new();
    let mut conversation_tasks = Vec::new();
    for slot in layout.enrolled_nodes() {
        let enrollment = EnrollmentFile::load(&layout.slot_enrollment(slot))?;
        let agents = recover_agents(&root, enrollment.node_id)?;
        conversation_tasks.push(tokio::spawn(
            conversation::Archiver {
                root: root.clone(),
                node: enrollment.node_id,
                link: ServerLink::from_enrollment(&enrollment)?,
                cfg: settings.host.docker_local.clone(),
            }
            .run(shutdown.clone()),
        ));
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
            let session = sessions
                .get(&node.id)
                .context("manual node session absent")?;
            let max_slots = if session.aware() {
                session.slots().clamp(1, 8)
            } else {
                legacy_max_slots
            };
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
            let request = json!({"providers":providers,"max_slots":max_slots,"busy":session.retired() || if session.aware() { session.full() } else { active.load(Ordering::SeqCst)>0 },"reports":reports});
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
                    login: None,
                    login_checked: None,
                });
                a.desired_running = job["desired_running"] == true;
                if !a.desired_running {
                    if a.sandbox.is_some() {
                        finish_login(&exe, &settings.host.docker_local, &node.link, a).await;
                    }
                    if let Some(sandbox) = &a.sandbox {
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
                    session.release_manual(id);
                    a.status = "stopped".into();
                    a.stage = "Stopped; workspace retained".into();
                    save(&root, a)?;
                    continue;
                }
                if a.sandbox.is_none() {
                    if session.retired() || session.slots() == 0 {
                        continue;
                    }
                    if session.aware() {
                        if !session.try_manual(id) {
                            pending = true;
                            continue;
                        }
                    } else if active.load(Ordering::SeqCst) > 0 {
                        continue;
                    }
                    if !memory_available() {
                        session.release_manual(id);
                        pending = true;
                        a.stage = "Waiting for memory to become available".into();
                        save(&root, a)?;
                        continue;
                    }
                    if let Err(error) =
                        create(&config, &settings, node.id, &node.link, a, &root).await
                    {
                        a.status = "failed".into();
                        if let Some(refused) = error.downcast_ref::<LoginRefused>() {
                            // Refused before any sandbox: the owner sees the server's reason.
                            session.release_manual(id);
                            a.stage = refused.0.chars().take(128).collect();
                            save(&root, a)?;
                            continue;
                        }
                        // Creation may have left a live sandbox: retain occupancy until a confirmed stop.
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
                let docker = &settings.host.docker_local;
                if a.login_check_due(login_check) {
                    a.login_checked = Some(std::time::Instant::now());
                    if let Err(error) = sync_login(&exe, docker, &node.link, a, true).await {
                        tracing::warn!(session_id=%id, %error, "Codex login sync");
                    }
                }
                match observe(&exe, docker, &node.link, a, node.lease_until).await {
                    Ok(Some(text)) => {
                        node.snapshots.insert(id, text);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        a.stage = "Reconnecting to the same terminal".into();
                    }
                }
                if a.status == "stopped" {
                    session.release_manual(id);
                }
                save(&root, a)?;
            }
            // v1 preserves its whole-node manual pause. For slot-aware nodes, waiting manual
            // launches, and launches the server still holds as pending, ask idle queued pollers
            // to yield their local permits. Without `pending` the server never assigns (the node
            // reports busy while pollers hold every permit) and nothing is ever waiting here.
            let waiting = node
                .agents
                .values()
                .any(|a| a.desired_running && a.sandbox.is_none() && a.status != "failed");
            let paused = if session.aware() {
                waiting || pending
            } else {
                pending
            };
            // Notify pollers only on a change: every notification woke them to poll.
            pause.send_if_modified(|v| std::mem::replace(v, paused) != paused);
        }
        tokio::select! {_=tokio::time::sleep(std::time::Duration::from_millis(500))=>{},_=shutdown.changed()=>{}}
    }
    for node in &mut nodes {
        let session = sessions
            .get(&node.id)
            .context("manual node session absent")?;
        let max_slots = if session.aware() {
            session.slots().clamp(1, 8)
        } else {
            legacy_max_slots
        };
        for a in node.agents.values_mut() {
            if a.sandbox.is_some() && a.status != "stopped" {
                finish_login(&exe, &settings.host.docker_local, &node.link, a).await;
            }
            if let Some(sandbox) = &a.sandbox {
                let mut backend =
                    DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
                if backend.stop(sandbox).await.is_confirmed() {
                    session.release_manual(a.id);
                    a.status = "stopped".into();
                    a.stage = "Worker stopped; workspace retained".into();
                    save(&root, a)?;
                }
            }
        }
        let _=node.link.manual_poll(&json!({"providers":providers,"max_slots":max_slots,"busy":true,"reports":node.agents.values().map(|a|a.report(None)).collect::<Vec<_>>()})).await;
    }
    for task in conversation_tasks {
        task.abort();
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
        // The install script is valid shell and takes Codex's lock before it writes.
        let checked = std::process::Command::new("sh")
            .args(["-n", "-c", INSTALL_AUTH])
            .status();
        assert!(checked.unwrap().success());
        let (lock, write) = (INSTALL_AUTH.find("flock 9"), INSTALL_AUTH.find("cat >"));
        assert!(lock.is_some() && lock < write);
        assert!(valid_slug("ElectricJack/aq-server"));
        assert!(!valid_slug("../aq-server"));
        assert!(!valid_slug("https://evil/repo"));
        assert!(!valid_slug("owner/repo\ncommand"));
    }
    fn login(refreshed: &str) -> Vec<u8> {
        json!({"auth_mode": "chatgpt", "last_refresh": refreshed,
            "tokens": {"id_token": "i", "access_token": "a", "refresh_token": format!("secret-{refreshed}"), "account_id": "acct"}})
        .to_string()
        .into_bytes()
    }
    /// A running Codex session against a login store and a sandbox that are both
    /// stand-ins: the store is the two session routes with compare-and-swap, the
    /// sandbox one file behind a program that answers the `exec` calls made here.
    #[tokio::test]
    async fn a_session_writes_its_refresh_back_and_installs_a_newer_stored_login() {
        use axum::{
            Json, Router,
            body::Bytes,
            extract::{Query, State},
            http::StatusCode,
            routing::post,
        };
        use std::os::unix::fs::PermissionsExt;
        type Store = Arc<std::sync::Mutex<(i64, Vec<u8>)>>;
        let (t0, t1, t2, t3) = (
            "2026-11-01T00:00:00Z",
            "2026-11-02T00:00:00Z",
            "2026-11-03T00:00:00Z",
            "2026-11-04T00:00:00Z",
        );
        let store: Store = Arc::new(std::sync::Mutex::new((3, login(t0))));
        let lease = |State(store): State<Store>| async move {
            let (version, bytes) = store.lock().unwrap().clone();
            ([("aq-material-version", version.to_string())], bytes)
        };
        let refresh = |State(store): State<Store>,
                       Query(q): Query<BTreeMap<String, i64>>,
                       body: Bytes| async move {
            let mut store = store.lock().unwrap();
            if q["base_version"] != store.0 {
                let lost = json!({"accepted": false, "version": store.0, "replayed": false});
                return (StatusCode::CONFLICT, Json(lost));
            }
            *store = (store.0 + 1, body.to_vec());
            let won = json!({"accepted": true, "version": store.0, "replayed": false});
            (StatusCode::OK, Json(won))
        };
        let app = Router::new()
            .route("/v1/worker/manual-agents/{id}/login", post(lease))
            .route("/v1/worker/manual-agents/{id}/login/refresh", post(refresh))
            .with_state(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let link = ServerLink::new(&origin, "node-credential-0123456789", None).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("engine");
        // argv: --remote --url URL exec [-i] CONTAINER PROGRAM ...
        std::fs::write(&exe, "#!/bin/sh\nf=$(dirname \"$0\")/auth.json\nif [ \"$5\" = -i ]; then cat > \"$f\"; elif [ \"$6\" = cat ]; then cat \"$f\"; else rm -f \"$f\"; fi\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sandbox = dir.path().join("auth.json");
        let cfg: DockerLocalConfig =
            serde_json::from_value(json!({"socket": "/nonexistent"})).unwrap();
        let id = Uuid::new_v4();
        let owner = Ownership {
            node_id: id,
            boot_id: id,
            attempt_id: id,
        };
        let sandbox_id: SandboxIdentity = serde_json::from_value(
            json!({"kind": "docker-local", "backend_id": "c", "owner": owner}),
        )
        .unwrap();
        let (leased, version) = link.session_login(id).await.unwrap();
        std::fs::write(&sandbox, &*leased).unwrap();
        let mut a = Agent {
            id,
            node_id: id,
            project_id: id,
            spec: json!({"provider": "openai"}),
            status: "running".into(),
            stage: String::new(),
            desired_running: true,
            sandbox: Some(sandbox_id),
            acknowledged: vec![],
            login: Some(LoginLease::of_session(&leased, id, version).unwrap()),
            login_checked: None,
        };
        let version = |a: &Agent| a.login.as_ref().unwrap().base_version;
        let stored = || store.lock().unwrap().clone();
        // Nothing changed anywhere: nothing is written.
        sync_login(&exe, &cfg, &link, &mut a, true).await.unwrap();
        assert_eq!((version(&a), stored()), (3, (3, login(t0))));
        // Codex refreshed in the sandbox: version 4 based on 3.
        std::fs::write(&sandbox, login(t1)).unwrap();
        sync_login(&exe, &cfg, &link, &mut a, true).await.unwrap();
        assert_eq!((version(&a), stored()), (4, (4, login(t1))));
        // Another copy refreshed and stored version 5: it replaces the sandbox's login.
        *store.lock().unwrap() = (5, login(t2));
        sync_login(&exe, &cfg, &link, &mut a, true).await.unwrap();
        assert_eq!(version(&a), 5);
        assert_eq!(std::fs::read(&sandbox).unwrap(), login(t2));
        // Both refreshed: ours is superseded, and the stored one is installed.
        std::fs::write(&sandbox, login(t3)).unwrap();
        *store.lock().unwrap() = (6, login(t2));
        sync_login(&exe, &cfg, &link, &mut a, true).await.unwrap();
        assert_eq!((version(&a), stored().0), (6, 6));
        assert_eq!(std::fs::read(&sandbox).unwrap(), login(t2));
        // The session's private state names the version and never holds a token.
        let state = serde_json::to_string(&a).unwrap();
        assert!(
            state.contains("\"base_version\":6")
                && !state.contains("secret-")
                && !state.contains("acct")
        );
        // At stop the last refresh is written back and the login leaves the sandbox.
        std::fs::write(&sandbox, login(t3)).unwrap();
        finish_login(&exe, &cfg, &link, &mut a).await;
        assert_eq!(stored(), (7, login(t3)));
        assert!(!sandbox.exists());
        assert!(a.login.is_none(), "written back once");
    }
}
