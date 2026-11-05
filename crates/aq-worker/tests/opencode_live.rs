//! I3: OpenCode 1.18.35 qualification in a real docker-local sandbox, driven by
//! the B5a node controller, on the owner-chosen local Ollama provider (no
//! credential). Live and `#[ignore]`d; see `docs/implementation/I3.md`.
//!
//! Environment:
//! - `AQ_DOCKER_LOCAL_SOCKET`: the dedicated sandbox daemon's API socket.
//! - `AQ_OPENCODE_LIVE_IMAGE`: `repo@sha256:...` of the derived opencode-rust
//!   image, already present in that daemon (pulled by digest).
//! - `AQ_OPENCODE_LIVE_RELAY_BIND`: the host address the sandbox reaches
//!   through its host gateway (pasta maps `169.254.1.2` to the host's primary
//!   address, not to loopback, so a loopback-only Ollama needs this relay).
//! - `AQ_OPENCODE_LIVE_MODEL` (default `qwen3.5:9b`), `AQ_OPENCODE_LIVE_OLLAMA`
//!   (default `127.0.0.1:11434`), `AQ_OPENCODE_LIVE_GATEWAY` (default
//!   `169.254.1.2`), `AQ_DOCKER_LOCAL_SHARED_ROOT_FS`,
//!   `AQ_OPENCODE_LIVE_EVIDENCE_DIR` (copies the read-back outputs there).
//!
//! The recipe (argv, environment, `opencode.json`) comes from the production
//! `OpenCodeAgentHome::install` under an `AuthorizedConfinement` for container
//! mode; only its private-home prefix is relocated into the sandbox.
#[path = "support/docker_live.rs"]
mod docker_live;
use aq_session_protocol::{ArtifactRef, Capability as HarnessCapability, Digest, HarnessSettings};
use aq_session_protocol::{ModelSettings, Text};
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use aq_worker::native::adapter::opencode::{
    self, FORBIDDEN_ENVIRONMENT, OPENCODE_CLI_VERSION, OpenCodePaths, OpenCodeRoute,
};
use aq_worker::native::runner::sandbox::confinement::{
    self as sandbox_confinement, AttemptSeparation, ExecutionConfinement, QualificationRecord,
    authorize,
};
use aq_worker::native::runner::sandbox::harness_config::OpenCodeAgentHome;
use docker_live::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

const HARNESS: &str = "opencode";
const MATERIAL: &str = "/aq/private/material";
const OUT: &str = "/aq/work/out";
const HOME: &str = "/aq/work/oc-home";
const WORKSPACE: &str = "/aq/work/repo";
const EXECUTABLE: &str = "/opt/aq/bin/opencode";
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Not a credential: the Ollama route has none. The controller still delivers
/// private material, which the runner deletes before any work.
const LEASE: &[u8] = b"aq-i3-ollama-route-holds-no-credential";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live OpenCode test"))
}

fn text(s: &str) -> Text {
    Text::new(s).unwrap()
}

fn settings(model: &str) -> (HarnessSettings, ModelSettings) {
    let artifact = ArtifactRef {
        id: text("i3-live"),
        version: text("1"),
        digest: Digest::of(b"i3-live"),
    };
    let harness = HarnessSettings {
        harness: text(HARNESS),
        cli_version: text(OPENCODE_CLI_VERSION),
        adapter: artifact.clone(),
        environment: artifact,
        required_capabilities: [
            "aq.harness.startup-context",
            "aq.harness.container-confinement",
            "aq.harness.stop",
        ]
        .map(|id| HarnessCapability {
            id: text(id),
            version: 1,
        })
        .to_vec(),
    };
    let model = ModelSettings {
        provider: text("ollama"),
        model: text(model),
        reasoning: text("medium"),
        max_output_tokens: 8192,
        spending_budget: None,
    };
    (harness, model)
}

/// The production recipe for the Ollama route, installed in a host temporary
/// home and relocated to [`HOME`]: `(argv, env, opencode.json, host home)`.
struct Installed {
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    config: Vec<u8>,
    host_home: PathBuf,
}

fn install(root: &Path, model: &str, gateway: Ipv4Addr, port: u16) -> Installed {
    let authorized = authorize(
        sandbox_confinement::ConfinementMode::Container,
        &ExecutionConfinement {
            permit_container: true,
            require_native: false,
        },
        &QualificationRecord::docker_local("aq-rust"),
        &AttemptSeparation {
            runner_uid: 1001,
            agent: aq_session_host::users::AgentIdentity {
                uid: 1002,
                gid: 1002,
            },
            credentials_attempt_scoped: true,
        },
    )
    .unwrap();
    let (harness, model) = settings(model);
    let host_home = root.join("oc-home");
    let recipe = OpenCodeAgentHome::install(
        &authorized,
        &harness,
        &model,
        OpenCodePaths {
            executable: Path::new(EXECUTABLE),
            workspace: Path::new(WORKSPACE),
            private_home: &host_home,
        },
        OpenCodeRoute::Ollama {
            host: gateway,
            port,
        },
        None,
    )
    .unwrap();
    assert_eq!(recipe.executable, Path::new(EXECUTABLE));
    assert_eq!(recipe.working_directory, Path::new(WORKSPACE));
    let config = std::fs::read(host_home.join(opencode::CONFIG_FILE)).unwrap();
    assert_eq!(
        config, recipe.config_json,
        "installed config is the recipe's"
    );
    let prefix = host_home.to_str().unwrap();
    let environment = recipe
        .environment
        .into_iter()
        .map(|(k, v)| {
            let v = match v.strip_prefix(prefix) {
                Some(rest) => format!("{HOME}{rest}"),
                None => v,
            };
            (k, v)
        })
        .collect();
    Installed {
        arguments: recipe.arguments,
        environment,
        config,
        host_home,
    }
}

/// Characters a relocated recipe value may contain, so it can be written into
/// the fixed runner without quoting.
fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._:=-+".contains(&b))
}

/// The fixed runner. It carries no secret; the recipe values are checked to be
/// shell-safe and the config is a quoted heredoc.
fn runner(installed: &Installed) -> String {
    let mut assignments = String::new();
    for (k, v) in &installed.environment {
        assert!(shell_safe(k) && shell_safe(v), "unsafe recipe value {k}");
        assignments.push_str(&format!(" {k}={v}"));
    }
    for a in &installed.arguments {
        assert!(shell_safe(a), "unsafe recipe argument {a}");
    }
    let arguments = installed.arguments.join(" ");
    let config = std::str::from_utf8(&installed.config).unwrap();
    assert!(!config.contains("AQ_OPENCODE_CONFIG_END"));
    format!(
        r#"set -uo pipefail
out={OUT}; mkdir -p "$out"
material={MATERIAL}
fail() {{ printf '{{"error":"%s"}}\n' "$1" > "$out/result.json"; exit 2; }}
[ -s "$material" ] || fail material_missing
rm -f "$material"
material_wiped=false; [ -e "$material" ] || material_wiped=true
home={HOME}
mkdir -m 700 "$home" "$home/data" || fail home
(umask 077; cat > "$home/opencode.json") <<'AQ_OPENCODE_CONFIG_END' || fail config
{config}
AQ_OPENCODE_CONFIG_END
oc_env=({assignments} )
export GIT_AUTHOR_NAME=aq-sandbox GIT_AUTHOR_EMAIL=sandbox@aq.invalid
export GIT_COMMITTER_NAME=aq-sandbox GIT_COMMITTER_EMAIL=sandbox@aq.invalid
git init -q --bare -b main /aq/work/publish.git || fail publish_init
repo={WORKSPACE}; mkdir -p "$repo/src"; cd "$repo" || fail repo
cat > Cargo.toml <<'TOML'
[package]
name = "qualify"
version = "0.1.0"
edition = "2024"
TOML
cat > src/lib.rs <<'RS'
/// Returns the sum of `a` and `b`.
pub fn add(a: u64, b: u64) -> u64 {{
    a - b
}}

#[cfg(test)]
mod tests {{
    #[test]
    fn adds() {{
        assert_eq!(super::add(2, 3), 5);
    }}
}}
RS
git init -q -b main && git add -A && git commit -qm baseline || fail baseline
base=$(git rev-parse HEAD)
red=false; cargo test --offline -q > "$out/red.log" 2>&1 || red=true
printf '%s' "The unit test in src/lib.rs fails. Fix the add function in src/lib.rs so that cargo test passes. Read src/lib.rs, then edit only src/lib.rs. Do not run any commands." \
  | timeout 900 env -i "${{oc_env[@]}}" {EXECUTABLE} {arguments} > "$out/opencode.jsonl" 2> "$out/opencode.err"
opencode_rc=$?
sid=$(sed -n 's/.*"sessionID":"\([A-Za-z0-9_]*\)".*/\1/p' "$out/opencode.jsonl" | head -n 1)
export_rc=0
timeout 120 env -i "${{oc_env[@]}}" {EXECUTABLE} export "$sid" > "$out/export.json" 2> "$out/export.err" || export_rc=$?
check_rc=0; cargo test --offline -q > "$out/check.log" 2>&1 || check_rc=$?
git diff --name-only "$base" > "$out/changed.txt"
git status --porcelain --untracked-files=all > "$out/status.txt"
publish_rc=0
{{ git commit -qam "fix add" && git push -q /aq/work/publish.git HEAD:refs/heads/main; }} > "$out/publish.log" 2>&1 || publish_rc=$?
head=$(git rev-parse HEAD)
published=$(git --git-dir=/aq/work/publish.git rev-parse refs/heads/main 2>/dev/null || echo none)
git --git-dir=/aq/work/publish.git log --format='%H %s' -n 2 refs/heads/main > "$out/published.txt" 2>&1
(cd "$home" && find . -type f | sort) > "$out/home-files.txt"
auth_file=false; [ -e "$home/data/opencode/auth.json" ] && auth_file=true
version=$(env -i "${{oc_env[@]}}" {EXECUTABLE} --version)
rm -rf "$home"
home_wiped=false; [ -e "$home" ] || home_wiped=true
cap_eff=$(awk '/^CapEff:/ {{print $2}}' /proc/self/status)
nnp=$(awk '/^NoNewPrivs:/ {{print $2}}' /proc/self/status)
printf '{{"cli_version":"%s","uid":%s,"cap_eff":"%s","no_new_privs":%s,"material_wiped":%s,"red":%s,"opencode_rc":%s,"session":"%s","export_rc":%s,"check_rc":%s,"publish_rc":%s,"base":"%s","head":"%s","published":"%s","auth_file":%s,"home_wiped":%s}}\n' \
  "$version" "$(id -u)" "$cap_eff" "$nnp" "$material_wiped" "$red" "$opencode_rc" "$sid" "$export_rc" "$check_rc" "$publish_rc" "$base" "$head" "$published" "$auth_file" "$home_wiped" > "$out/result.json"
[ "$opencode_rc" = 0 ] && [ "$export_rc" = 0 ] && [ "$check_rc" = 0 ] && [ "$publish_rc" = 0 ]
"#
    )
}

/// A relay from the sandbox's host gateway to the loopback-only Ollama,
/// alive only for the test. Only connections from `bind` itself (which is
/// how pasta presents the sandbox) are forwarded. Ollama refuses a non-local
/// `Host` (403, its DNS-rebinding guard), so the request head is rewritten to
/// the loopback upstream with `Connection: close`: one request per connection,
/// so every request head passes through the rewrite.
async fn relay(bind: Ipv4Addr, upstream: SocketAddr) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((bind, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut inbound, peer)) = listener.accept().await else {
                continue;
            };
            if peer.ip() != std::net::IpAddr::V4(bind) {
                eprintln!("relay refused {peer}");
                continue;
            }
            tokio::spawn(async move {
                let _ = forward(&mut inbound, upstream).await;
            });
        }
    });
    (port, task)
}

async fn forward(inbound: &mut tokio::net::TcpStream, upstream: SocketAddr) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let end = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if head.len() > 64 * 1024 {
            return Ok(());
        }
        let mut chunk = [0; 4096];
        let n = inbound.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
    };
    let text = String::from_utf8_lossy(&head[..end]).into_owned();
    let mut rewritten = String::new();
    for (i, line) in text.split("\r\n").enumerate() {
        let name = line
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if i > 0 && (name == "host" || name == "connection") {
            continue;
        }
        rewritten.push_str(line);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str(&format!("Host: {upstream}\r\nConnection: close\r\n\r\n"));
    let mut outbound = tokio::net::TcpStream::connect(upstream).await?;
    outbound.write_all(rewritten.as_bytes()).await?;
    outbound.write_all(&head[end + 4..]).await?;
    tokio::io::copy_bidirectional(inbound, &mut outbound).await?;
    Ok(())
}

/// Run one exported session through a reader: `(usages, tools started, tool
/// results, text roles, turn boundaries, providers)`.
#[derive(Debug, Default, PartialEq)]
struct Read {
    usages: usize,
    started: usize,
    results: usize,
    roles: Vec<String>,
    turns: usize,
    providers: Vec<String>,
}

fn binding() -> Value {
    json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})
}

fn frames(export: &Value) -> Vec<Vec<u8>> {
    export["messages"]
        .as_array()
        .expect("export messages")
        .iter()
        .map(|m| {
            let mut bytes = serde_json::to_vec(m).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}

macro_rules! tally {
    ($out:expr, $obs:ident) => {{
        #[allow(unused_imports)]
        use $obs::lifecycle::*;
        let mut read = Read::default();
        for record in &$out {
            record.validate().unwrap();
            match &record.body.kind {
                ObservationKind::Usage { usage } => {
                    read.usages += 1;
                    read.providers.push(usage.call.provider.as_str().to_owned());
                }
                ObservationKind::ToolStarted { .. } => read.started += 1,
                ObservationKind::ToolResult { .. } => read.results += 1,
                ObservationKind::TextReference { role, .. } => read.roles.push(format!("{role:?}")),
                ObservationKind::TurnBoundary { .. } => read.turns += 1,
                _ => {}
            }
        }
        read
    }};
}

fn read_with_observation(export: &Value, session: &str) -> Read {
    use aq_observation as obs;
    let source = obs::Source {
        binding: serde_json::from_value(binding()).unwrap(),
        source_id: Uuid::from_u128(8),
        generation: 1,
        native_session_key: text(session),
        format: obs::Format::OpenCodeSessionDto6c24ec9,
        provider: text("ollama"),
        account_reference: text("local-ollama"),
        registered_session_key: None,
    };
    let mut state = obs::Normalizer::new(source, 128)
        .unwrap()
        .bind_scoped_record_channel(session)
        .unwrap();
    let mut out = vec![];
    for bytes in frames(export) {
        let frame = obs::framing::Frame {
            start: 0,
            bytes,
            oversized: false,
        };
        let (next, events) = state.apply(&frame, 100).unwrap();
        state = next;
        out.extend(events);
    }
    tally!(out, obs)
}

/// The engine reader against an in-memory row view.
#[derive(Default)]
struct Rows {
    tables: BTreeMap<aq_observation_engine::rows::Table, BTreeMap<String, Value>>,
    display: BTreeMap<u64, String>,
}
impl aq_observation_engine::rows::ReadView for Rows {
    fn get(&self, t: aq_observation_engine::rows::Table, k: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.tables.get(&t).and_then(|v| v.get(k)).cloned())
    }
    fn count(&self, t: aq_observation_engine::rows::Table) -> anyhow::Result<usize> {
        Ok(self.tables.get(&t).map_or(0, |v| v.len()))
    }
    fn display_suffix(&self, start: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        Ok(self
            .display
            .range(start..)
            .take(limit)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }
}

fn read_with_engine(export: &Value, session: &str) -> Read {
    use aq_observation_engine::{self as obs, rows::Table};
    let source = obs::Source {
        binding: serde_json::from_value(binding()).unwrap(),
        source_id: Uuid::from_u128(8),
        generation: 1,
        native_session_key: text(session),
        format: obs::Format::OpenCodeSessionDto6c24ec9,
        provider: text("ollama"),
        account_reference: text("local-ollama"),
        registered_session_key: None,
    };
    let mut context = obs::Context::new(source, 128)
        .unwrap()
        .bind_scoped_record_channel(session)
        .unwrap();
    let mut rows = Rows::default();
    let mut out = vec![];
    for bytes in frames(export) {
        let frame = obs::framing::Frame {
            start: 0,
            bytes,
            oversized: false,
        };
        let delta = obs::normalize(&context, &rows, &frame, 100).unwrap();
        context = delta.context;
        for ((table, key), value) in delta.rows.writes {
            if table == Table::DisplayByOrdinal {
                let ordinal = key.parse().unwrap();
                match &value {
                    Some(v) => {
                        rows.display
                            .insert(ordinal, serde_json::from_value(v.clone()).unwrap());
                    }
                    None => {
                        rows.display.remove(&ordinal);
                    }
                }
            }
            let map = rows.tables.entry(table).or_default();
            match value {
                Some(v) => map.insert(key, v),
                None => map.remove(&key),
            };
        }
        out.extend(delta.events);
    }
    tally!(out, obs)
}

#[test]
fn docker_local_offers_container_confinement_only() {
    assert!(require_confinement(ConfinementMode::Container).is_ok());
    assert!(require_confinement(ConfinementMode::Native).is_err());
}

#[test]
fn runner_uses_the_installed_recipe_and_carries_no_credential() {
    let root = tempfile::tempdir().unwrap();
    let installed = install(root.path(), "qwen3.5:9b", Ipv4Addr::new(169, 254, 1, 2), 9);
    let host = installed.host_home.to_str().unwrap();
    for (k, v) in &installed.environment {
        assert!(!FORBIDDEN_ENVIRONMENT.contains(&k.as_str()), "{k}");
        assert!(!v.contains(host), "{k} still names the host home");
    }
    assert_eq!(installed.environment["HOME"], HOME);
    assert_eq!(
        installed.environment["OPENCODE_CONFIG"],
        format!("{HOME}/opencode.json")
    );
    assert_eq!(
        installed.environment["OPENCODE_DISABLE_DEFAULT_PLUGINS"],
        "1"
    );
    assert_eq!(
        installed.arguments,
        [
            "run",
            "--format",
            "json",
            "--pure",
            "--model",
            "ollama/qwen3.5:9b"
        ]
    );
    // No credential file of any kind in the installed home.
    let mut files = vec![];
    for entry in walk(&installed.host_home) {
        files.push(entry.strip_prefix(&installed.host_home).unwrap().to_owned());
    }
    assert_eq!(files, [PathBuf::from("opencode.json")]);
    let config: Value = serde_json::from_slice(&installed.config).unwrap();
    assert_eq!(config["enabled_providers"], json!(["ollama"]));
    assert_eq!(
        config["provider"]["ollama"]["options"]["baseURL"],
        "http://169.254.1.2:9/v1"
    );
    let script = runner(&installed);
    assert!(!script.contains("auth.json\")"), "runner writes no login");
    assert!(!script.contains("--dangerously"));
    let wipe = script.find("rm -f \"$material\"").unwrap();
    assert!(
        wipe < script.find("git init").unwrap(),
        "material wiped first"
    );
    assert!(
        script.contains("env -i \"${oc_env[@]}\" /opt/aq/bin/opencode run --format json --pure")
    );
    assert!(script.contains("rm -rf \"$home\""), "home wiped at the end");
    assert!(!contains(script.as_bytes(), LEASE));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the opencode-rust image and a local Ollama"]
async fn live_opencode_ollama_edit_check_publish_cycle() {
    let socket = PathBuf::from(env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = env("AQ_OPENCODE_LIVE_IMAGE");
    let bind: Ipv4Addr = env("AQ_OPENCODE_LIVE_RELAY_BIND").parse().unwrap();
    let model = std::env::var("AQ_OPENCODE_LIVE_MODEL").unwrap_or_else(|_| "qwen3.5:9b".into());
    let upstream: SocketAddr = std::env::var("AQ_OPENCODE_LIVE_OLLAMA")
        .unwrap_or_else(|_| "127.0.0.1:11434".into())
        .parse()
        .unwrap();
    assert!(upstream.ip().is_loopback(), "relay only to a local Ollama");
    let gateway: Ipv4Addr = std::env::var("AQ_OPENCODE_LIVE_GATEWAY")
        .unwrap_or_else(|_| "169.254.1.2".into())
        .parse()
        .unwrap();
    let evidence = std::env::var_os("AQ_OPENCODE_LIVE_EVIDENCE_DIR").map(PathBuf::from);
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();

    let (port, relay_task) = relay(bind, upstream).await;
    let dir = tempfile::tempdir().unwrap();
    let installed = install(dir.path(), &model, gateway, port);

    let mut config = DockerLocalConfig::new(socket);
    config.templates.push(AllowedTemplate {
        image: image.clone(),
        digest: digest.clone(),
    });
    config.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), runner(&installed)]);
    config.api_timeout_ms = 60_000;
    config.stop_grace_secs = 5;
    config.shared_root_fs = std::env::var_os("AQ_DOCKER_LOCAL_SHARED_ROOT_FS").is_some();
    assert_eq!(config.material_path, MATERIAL);
    let mut backend = DockerLocalBackend::connect_unix(config)
        .await
        .expect("connect to the dedicated sandbox daemon");
    backend
        .pull_template(&digest)
        .await
        .expect("opencode-rust image present by digest");
    let shared = Shared(Arc::new(Mutex::new(backend)));

    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = Controller::open(
        &dir.path().join("controller"),
        Uuid::new_v4(),
        node(&digest, HARNESS, OPENCODE_CLI_VERSION),
        shared.clone(),
        Lease {
            token: Zeroizing::new(LEASE.to_vec()),
            deadline: deadline.clone(),
        },
    )
    .unwrap();
    c.register().await.unwrap();
    let request = launch(
        c.identity(),
        &digest,
        HARNESS,
        OPENCODE_CLI_VERSION,
        RUN_TIMEOUT,
    );
    deadline.store(request.hard_deadline_ms, Ordering::SeqCst);
    let id = c.reserve(request).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    c.launch(id, &()).await.expect("controller launch");
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();

    let started = Instant::now();
    let exit = loop {
        match shared.0.lock().await.runner_exit(&sandbox).await {
            Ok(exit) => break Some(exit),
            Err(_) if started.elapsed() < RUN_TIMEOUT => {}
            Err(_) => break None,
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let elapsed = started.elapsed();

    let mut outputs = BTreeMap::new();
    for name in [
        "result.json",
        "opencode.jsonl",
        "opencode.err",
        "export.json",
        "export.err",
        "red.log",
        "check.log",
        "changed.txt",
        "status.txt",
        "publish.log",
        "published.txt",
        "home-files.txt",
    ] {
        let bytes = read(&shared, &sandbox, &format!("{OUT}/{name}")).await;
        outputs.insert(name, bytes.unwrap_or_default());
    }
    let source = read(&shared, &sandbox, &format!("{WORKSPACE}/src/lib.rs"))
        .await
        .unwrap_or_default();
    let material_gone = read(&shared, &sandbox, MATERIAL).await.is_none();
    let home_gone = read(&shared, &sandbox, &format!("{HOME}/opencode.json"))
        .await
        .is_none();

    // Teardown before any outcome assertion, so a failing run still wipes.
    let stopped = c.stop(id).await.unwrap().is_confirmed();
    let disk_after_stop = c.inspect(id).await.unwrap().disk_present;
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "i3-live").unwrap();
    let destroyed = c.destroy(id, &release).await.unwrap().is_confirmed();
    let owned = shared
        .0
        .lock()
        .await
        .list_owned(c.identity().node_id)
        .await
        .unwrap();
    relay_task.abort();

    if let Some(dir) = &evidence {
        std::fs::create_dir_all(dir).unwrap();
        for (name, bytes) in &outputs {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        std::fs::write(dir.join("lib.rs"), &source).unwrap();
        std::fs::write(dir.join("opencode.json"), &installed.config).unwrap();
    }
    assert!(stopped, "stop confirmed");
    assert!(disk_after_stop, "stop preserves the work volume");
    assert!(destroyed, "destroy confirmed");
    assert!(owned.is_empty(), "teardown left {owned:?}");

    let result: Value = serde_json::from_slice(&outputs["result.json"]).unwrap_or(Value::Null);
    eprintln!("runner exit {exit:?} after {elapsed:?}: {result}");
    assert_eq!(exit, Some(RunnerExit::Exited(0)), "result: {result}");
    assert_eq!(result["cli_version"], OPENCODE_CLI_VERSION);
    assert_eq!(result["cap_eff"], "0000000000000000", "no capabilities");
    assert_eq!(result["no_new_privs"], 1);
    assert_eq!(result["material_wiped"], true);
    assert!(
        material_gone,
        "material file absent after the lease is read"
    );
    assert_eq!(result["red"], true, "baseline test fails before the edit");
    assert_eq!(result["check_rc"], 0);
    assert_eq!(result["publish_rc"], 0);
    assert_eq!(result["published"], result["head"]);
    assert_ne!(result["head"], result["base"]);
    assert_eq!(
        String::from_utf8_lossy(&outputs["changed.txt"]).trim(),
        "src/lib.rs"
    );
    assert!(String::from_utf8_lossy(&source).contains("a + b"));
    assert_eq!(result["auth_file"], false, "no login written");
    assert_eq!(
        String::from_utf8_lossy(&outputs["home-files.txt"])
            .lines()
            .filter(|l| l.contains("auth"))
            .count(),
        0,
        "no credential file in the home"
    );
    assert_eq!(result["home_wiped"], true);
    assert!(home_gone, "private home wiped");

    // Both readers on the real session export.
    let session = result["session"].as_str().unwrap();
    assert!(session.starts_with("ses_"), "session id {session}");
    let raw = &outputs["export.json"];
    let start = raw.iter().position(|b| *b == b'{').expect("export JSON");
    let export: Value = serde_json::from_slice(&raw[start..]).unwrap();
    assert_eq!(export["info"]["id"], session);
    let first = read_with_observation(&export, session);
    let engine = read_with_engine(&export, session);
    eprintln!("readers: {first:?}");
    assert_eq!(first, engine, "both readers agree");
    assert!(first.usages >= 1, "usage per assistant message");
    assert!(first.providers.iter().all(|p| p == "ollama"));
    assert!(
        first.started >= 2 && first.started == first.results,
        "read and edit tools"
    );
    assert_eq!(first.roles.first().map(String::as_str), Some("User"));
    assert_eq!(first.turns, 1, "one turn boundary at the final stop");
}
