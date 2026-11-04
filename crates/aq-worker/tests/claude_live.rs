//! I4a (re-scoped 2026-11-04): Claude Code CLI qualification with the owner's
//! Claude subscription login in a real docker-local sandbox, driven by the B5a
//! node controller. Live and `#[ignore]`d; see `docs/implementation/I4a.md`.
//!
//! Environment:
//! - `AQ_DOCKER_LOCAL_SOCKET`: the dedicated sandbox daemon's API socket.
//! - `AQ_CLAUDE_LIVE_IMAGE`: `repo@sha256:...` of the derived claude-rust image,
//!   already present in that daemon (pulled by digest).
//! - `AQ_CLAUDE_LIVE_TOKEN_FILE`: the owner's 0600 `claude setup-token` file.
//! - `AQ_CLAUDE_LIVE_CLI_VERSION` (default 2.1.318), `AQ_DOCKER_LOCAL_SHARED_ROOT_FS`,
//!   `AQ_CLAUDE_LIVE_EVIDENCE_DIR` (copies the read-back outputs there).
//!
//! The token reaches the sandbox only as controller-delivered private
//! material: never argv, labels, create env or logs. The runner moves it into
//! `CLAUDE_CODE_OAUTH_TOKEN`, deletes the file and never writes it back.
use aq_admission_sandbox::sandbox::{
    DelegatedPermit, HarnessVersion, Money, Registration, TemplateAdvertisement,
};
use aq_admission_sandbox::transport::WorkerIdentity;
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, SessionIdentity, Text};
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
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

const DEFAULT_CLI_VERSION: &str = "2.1.318";
const HARNESS: &str = "claude-code";
const MATERIAL: &str = "/aq/private/material";
const OUT: &str = "/aq/work/out";
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The fixed runner. It carries no secret: the token is read from the
/// delivered material file, which is deleted before anything else runs.
const RUNNER: &str = r#"set -uo pipefail
out=/aq/work/out; mkdir -p "$out"
material=/aq/private/material
fail() { printf '{"error":"%s"}\n' "$1" > "$out/result.json"; exit 2; }
[ -s "$material" ] || fail material_missing
CLAUDE_CODE_OAUTH_TOKEN="$(cat "$material")"; export CLAUDE_CODE_OAUTH_TOKEN
rm -f "$material"
material_wiped=false; [ -e "$material" ] || material_wiped=true
export GIT_AUTHOR_NAME=aq-sandbox GIT_AUTHOR_EMAIL=sandbox@aq.invalid
export GIT_COMMITTER_NAME=aq-sandbox GIT_COMMITTER_EMAIL=sandbox@aq.invalid
git init -q --bare -b main /aq/work/publish.git || fail publish_init
repo=/aq/work/repo; mkdir -p "$repo/src"; cd "$repo" || fail repo
cat > Cargo.toml <<'TOML'
[package]
name = "qualify"
version = "0.1.0"
edition = "2024"
TOML
cat > src/lib.rs <<'RS'
/// Returns the sum of `a` and `b`.
pub fn add(a: u64, b: u64) -> u64 {
    a - b
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(2, 3), 5);
    }
}
RS
git init -q -b main && git add -A && git commit -qm baseline || fail baseline
base=$(git rev-parse HEAD)
red=false; cargo test --offline -q > "$out/red.log" 2>&1 || red=true
timeout 900 claude -p "The unit test in src/lib.rs fails. Fix the add function in src/lib.rs so that cargo test passes. Edit only src/lib.rs and do not run any commands." \
  --permission-mode acceptEdits --allowedTools Read,Edit --max-turns 12 \
  --output-format stream-json --verbose > "$out/claude.jsonl" 2> "$out/claude.err"
claude_rc=$?
check_rc=0; cargo test --offline -q > "$out/check.log" 2>&1 || check_rc=$?
git diff --name-only "$base" > "$out/changed.txt"
publish_rc=0
{ git commit -qam "fix add" && git push -q /aq/work/publish.git HEAD:refs/heads/main; } > "$out/publish.log" 2>&1 || publish_rc=$?
head=$(git rev-parse HEAD)
published=$(git --git-dir=/aq/work/publish.git rev-parse refs/heads/main 2>/dev/null || echo none)
git --git-dir=/aq/work/publish.git log --format='%H %s' -n 2 refs/heads/main > "$out/published.txt" 2>&1
# Leak scan: the pattern comes from a pipe, never from argv.
leaks=$(printenv CLAUDE_CODE_OAUTH_TOKEN | grep -rlF -f - "$HOME" /aq /tmp 2>/dev/null | wc -l)
credentials=false; [ -e "$HOME/.claude/.credentials.json" ] && credentials=true
unset CLAUDE_CODE_OAUTH_TOKEN
version=$(claude --version)
cap_eff=$(awk '/^CapEff:/ {print $2}' /proc/self/status)
nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)
printf '{"cli_version":"%s","uid":%s,"cap_eff":"%s","no_new_privs":%s,"material_wiped":%s,"red":%s,"claude_rc":%s,"check_rc":%s,"publish_rc":%s,"base":"%s","head":"%s","published":"%s","token_files":%s,"credentials_file":%s}\n' \
  "$version" "$(id -u)" "$cap_eff" "$nnp" "$material_wiped" "$red" "$claude_rc" "$check_rc" "$publish_rc" "$base" "$head" "$published" "$leaks" "$credentials" > "$out/result.json"
[ "$claude_rc" = 0 ] && [ "$check_rc" = 0 ] && [ "$publish_rc" = 0 ]
"#;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live Claude test"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The owner's long-lived token: a private regular file, trimmed, never logged.
fn read_token(path: &Path) -> Zeroizing<Vec<u8>> {
    let meta = std::fs::metadata(path).expect("token file");
    assert!(meta.is_file(), "token path must be a regular file");
    assert_eq!(
        meta.permissions().mode() & 0o077,
        0,
        "token file must not be group/world readable"
    );
    let raw = Zeroizing::new(std::fs::read(path).expect("read token"));
    let token = Zeroizing::new(raw.trim_ascii().to_vec());
    assert!(!token.is_empty(), "token file is empty");
    token
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The controller owns the backend; the test keeps a handle to read the
/// sandbox's outputs and observe the runner's exit.
#[derive(Clone)]
struct Shared(Arc<Mutex<DockerLocalBackend>>);

impl PlacementBackend for Shared {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerLocal
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::PersistentDiskDuringAttempt]
    }
    async fn create(&mut self, r: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        self.0.lock().await.create(r).await
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> Result<(), BackendError> {
        self.0.lock().await.deliver_private_material(s, m).await
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        self.0.lock().await.start_runner(s).await
    }
    async fn inspect(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        self.0.lock().await.inspect(s).await
    }
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        self.0.lock().await.list_owned(node).await
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        self.0.lock().await.stop(s).await
    }
    async fn runner_exit(&mut self, s: &SandboxIdentity) -> Result<RunnerExit, BackendError> {
        self.0.lock().await.runner_exit(s).await
    }
    fn disk_limit(&self) -> DiskLimitStatus {
        self.0.try_lock().expect("backend idle").disk_limit()
    }
    async fn disk_usage(&mut self, node: Uuid) -> Result<Vec<DiskUsage>, BackendError> {
        self.0.lock().await.disk_usage(node).await
    }
    async fn measured_disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        self.0.lock().await.measured_disk_usage(preserved).await
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        self.0.lock().await.destroy(s, a).await
    }
}

/// Test credential lease: a preparation permit bound to the attempt, and the
/// subscription token as the private payload. No refresh, no write-back.
struct Lease {
    token: Zeroizing<Vec<u8>>,
    /// The reservation's hard deadline, which the permit must equal; set once
    /// the request is built (after the controller owns the provider).
    deadline: Arc<AtomicU64>,
}

impl PermitProvider for Lease {
    type Request = ();
    async fn register(&mut self, _: &Registration) -> anyhow::Result<()> {
        Ok(())
    }
    async fn issue(
        &mut self,
        _: &(),
        sandbox: &SandboxIdentity,
        manifest: &LaunchManifest,
    ) -> anyhow::Result<IssuedMaterial> {
        let deadline = self.deadline.load(Ordering::SeqCst);
        assert!(deadline > now_ms(), "lease deadline not set");
        let permit: DelegatedPermit = serde_json::from_value(json!({
            "schema_version": 1,
            "permit_id": Uuid::new_v4(),
            "credential": "opaque_secret_01234567890123456789012345",
            "binding": {
                "attempt_id": sandbox.owner.attempt_id,
                "grant_generation": manifest.bindings.assignment_generation,
                "sandbox_id": Uuid::new_v4(),
                "node": {"node_id": sandbox.owner.node_id, "boot_id": sandbox.owner.boot_id},
                "channel_id": Uuid::new_v4(),
            },
            "scope": "preparation",
            "issued_at_ms": now_ms() - 10,
            "expires_at_ms": deadline,
            "hard_deadline_ms": deadline,
        }))?;
        Ok(IssuedMaterial {
            permit,
            payload: PrivateMaterial::new(self.token.to_vec()),
        })
    }
}

fn launch(identity: &WorkerIdentity, digest: &TemplateDigest, cli: &str) -> LaunchRequest {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../aq-session-protocol/artifacts/fixtures.json"
    ))
    .unwrap();
    let mut m: LaunchManifest = serde_json::from_value(
        fixtures["valid_documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "launch")
            .unwrap()["body"]["manifest"]
            .clone(),
    )
    .unwrap();
    m.bindings.attempt_id = Uuid::new_v4();
    m.bindings.session = SessionIdentity::Remote {
        node_id: identity.node_id,
        boot_id: identity.boot_id,
        session_id: Uuid::new_v4(),
        attempt_id: m.bindings.attempt_id,
        assignment_generation: m.bindings.assignment_generation,
    };
    m.harness.harness = Text::new(HARNESS).unwrap();
    m.harness.cli_version = Text::new(cli).unwrap();
    m.harness.environment.digest =
        serde_json::from_value(json!(digest.as_str().strip_prefix("sha256:").unwrap())).unwrap();
    m.resources.cpu_millis = 2000;
    m.resources.memory_bytes = 4 << 30;
    m.resources.process_limit = 512;
    m.resources.wall_time_ms = RUN_TIMEOUT.as_millis() as u64;
    m.resources.stop_grace_ms = 5_000;
    LaunchRequest {
        manifest: m,
        hard_deadline_ms: now_ms() + RUN_TIMEOUT.as_millis() as u64 + 60_000,
        disk_bytes: 4 << 30,
        required_capabilities: vec![Capability::PersistentDiskDuringAttempt],
    }
}

fn node(digest: &TemplateDigest, cli: &str) -> NodeConfig {
    NodeConfig {
        preserve: Default::default(),
        runtime: ArtifactRef {
            id: Text::new("runtime").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"runtime"),
        },
        capabilities: vec![],
        features: vec![],
        templates: vec![TemplateAdvertisement {
            digest: serde_json::from_value(json!(digest.as_str().strip_prefix("sha256:").unwrap()))
                .unwrap(),
            harnesses: vec![HarnessVersion {
                harness: HARNESS.into(),
                version: cli.into(),
            }],
            estimated_cold_start_ms: 30_000,
            estimated_cold_start_cost: Money {
                currency: "USD".into(),
                micros: 0,
            },
        }],
        max_concurrent_sandboxes: 1,
        spend_ceiling: None,
        capacity: CapacityLimits {
            memory_bytes: 8 << 30,
            cpu_millis: 4000,
            disk_bytes: 16 << 30,
            max_preparing: 1,
        },
        recovery: RecoveryConfig::default(),
    }
}

/// Fingerprint of the host's own Claude login, which the sandbox must never
/// refresh or rewrite (no shared refresh lineage).
fn host_login_fingerprint() -> Option<(u64, std::time::SystemTime)> {
    let path = PathBuf::from(std::env::var_os("HOME")?).join(".claude/.credentials.json");
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

#[test]
fn docker_local_offers_container_confinement_only() {
    assert!(require_confinement(ConfinementMode::Container).is_ok());
    assert!(require_confinement(ConfinementMode::Native).is_err());
}

#[test]
fn runner_carries_no_secret_and_wipes_the_material() {
    // The fixed runner is public configuration: it may name the env variable
    // but never carries a token value, and it deletes the material first.
    assert!(!RUNNER.contains("sk-ant-"));
    let wipe = RUNNER.find("rm -f \"$material\"").unwrap();
    let first_use = RUNNER.find("git init").unwrap();
    assert!(
        wipe < first_use,
        "material must be deleted before work starts"
    );
    assert!(
        RUNNER.contains("grep -rlF -f -"),
        "leak scan pattern via pipe"
    );
    assert!(!RUNNER.contains("--dangerously-skip-permissions"));
}

#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the claude-rust image and the owner's token"]
async fn live_claude_subscription_edit_check_publish_cycle() {
    let socket = PathBuf::from(env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = env("AQ_CLAUDE_LIVE_IMAGE");
    let cli =
        std::env::var("AQ_CLAUDE_LIVE_CLI_VERSION").unwrap_or_else(|_| DEFAULT_CLI_VERSION.into());
    let token = read_token(Path::new(&env("AQ_CLAUDE_LIVE_TOKEN_FILE")));
    let evidence = std::env::var_os("AQ_CLAUDE_LIVE_EVIDENCE_DIR").map(PathBuf::from);
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let host_login = host_login_fingerprint();

    let mut config = DockerLocalConfig::new(socket);
    config.templates.push(AllowedTemplate {
        image: image.clone(),
        digest: digest.clone(),
    });
    config.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), RUNNER.into()]);
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
        .expect("claude-rust image present by digest");
    let shared = Shared(Arc::new(Mutex::new(backend)));

    let dir = tempfile::tempdir().unwrap();
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = Controller::open(
        &dir.path().join("controller"),
        Uuid::new_v4(),
        node(&digest, &cli),
        shared.clone(),
        Lease {
            token: token.clone(),
            deadline: deadline.clone(),
        },
    )
    .unwrap();
    c.register().await.unwrap();
    let request = launch(c.identity(), &digest, &cli);
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

    let mut outputs = std::collections::BTreeMap::new();
    for name in [
        "result.json",
        "claude.jsonl",
        "claude.err",
        "red.log",
        "check.log",
        "changed.txt",
        "publish.log",
        "published.txt",
    ] {
        let bytes = read(&shared, &sandbox, &format!("{OUT}/{name}")).await;
        outputs.insert(name, bytes.unwrap_or_default());
    }
    let source = read(&shared, &sandbox, "/aq/work/repo/src/lib.rs")
        .await
        .unwrap_or_default();
    let material_gone = read(&shared, &sandbox, MATERIAL).await.is_none();

    // Teardown before any outcome assertion, so a failing run still wipes:
    // the stopped-writer proof preserves the volume, then destroy removes the
    // container, the work volume and the (already deleted) material.
    let stopped = c.stop(id).await.unwrap().is_confirmed();
    let disk_after_stop = c.inspect(id).await.unwrap().disk_present;
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "i4a-live").unwrap();
    let destroyed = c.destroy(id, &release).await.unwrap().is_confirmed();
    let owned = shared
        .0
        .lock()
        .await
        .list_owned(c.identity().node_id)
        .await
        .unwrap();

    if let Some(dir) = &evidence {
        std::fs::create_dir_all(dir).unwrap();
        for (name, bytes) in &outputs {
            if !contains(bytes, &token) {
                std::fs::write(dir.join(name), bytes).unwrap();
            }
        }
        std::fs::write(dir.join("lib.rs"), &source).unwrap();
    }
    for (name, bytes) in &outputs {
        assert!(!contains(bytes, &token), "{name} carries the token");
    }
    assert!(stopped, "stop confirmed");
    assert!(disk_after_stop, "stop preserves the work volume");
    assert!(destroyed, "destroy confirmed");
    assert!(owned.is_empty(), "teardown left {owned:?}");
    assert_eq!(host_login_fingerprint(), host_login, "host login untouched");

    let result: Value = serde_json::from_slice(&outputs["result.json"]).unwrap_or(Value::Null);
    eprintln!("runner exit {exit:?} after {elapsed:?}: {result}");
    assert_eq!(exit, Some(RunnerExit::Exited(0)), "result: {result}");
    assert_eq!(result["cli_version"], format!("{cli} (Claude Code)"));
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
    assert_eq!(result["token_files"], 0, "token written to disk");
    assert_eq!(
        result["credentials_file"], false,
        "no credential write-back"
    );
    let transcript = String::from_utf8_lossy(&outputs["claude.jsonl"]);
    assert!(
        transcript.contains(r#""name":"Edit""#),
        "Claude used the Edit tool"
    );
    assert!(
        transcript
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|v| v["type"] == "result" && v["is_error"] == false),
        "Claude reported success"
    );
}

async fn read(shared: &Shared, sandbox: &SandboxIdentity, path: &str) -> Option<Vec<u8>> {
    shared.0.lock().await.read_file(sandbox, path).await.ok()
}
