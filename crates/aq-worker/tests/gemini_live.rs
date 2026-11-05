//! I2: Gemini CLI 0.67.0 qualification in a real docker-local sandbox, driven
//! by the B5a node controller. Live tests are `#[ignore]`d; see
//! `docs/implementation/I2.md`.
//!
//! Environment:
//! - `AQ_DOCKER_LOCAL_SOCKET`: the dedicated sandbox daemon's API socket.
//! - `AQ_GEMINI_LIVE_IMAGE`: `repo@sha256:...` of the derived gemini-rust image
//!   (`docs/implementation/I2-evidence/Containerfile.gemini-rust`), present in
//!   that daemon.
//! - `AQ_GEMINI_LIVE_KEY_FILE`: 0600 Gemini API key file (API-key test).
//! - `AQ_GEMINI_LIVE_OAUTH_CREDS_FILE`: 0600 `oauth_creds.json` captured by the
//!   enrollment procedure in I2.md (subscription test). Refreshed credentials
//!   are written back to it.
//! - `AQ_GEMINI_LIVE_MODEL` (default gemini-2.5-flash),
//!   `AQ_DOCKER_LOCAL_SHARED_ROOT_FS`, `AQ_GEMINI_LIVE_EVIDENCE_DIR`.
//!
//! The credential reaches the sandbox only as controller-delivered private
//! material: never argv, labels, create env or logs. With an API key the runner
//! hands the material to the in-sandbox `gemini-proxy` and deletes it; the CLI
//! sees only the proxy origin and its attempt-local capability (SBX7). With a
//! subscription the runner installs `.gemini/oauth_creds.json` (SBX19) and the
//! host reads it back after the run (C6b write-back) before destroy wipes it.
#[path = "support/docker_live.rs"]
mod docker_live;
use aq_session_protocol::{ArtifactRef, Capability, Digest, HarnessSettings, ModelSettings, Text};
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use aq_worker::native::adapter::gemini::{GEMINI_CLI_VERSION, GeminiAuth, GeminiPaths};
use aq_worker::native::runner::sandbox::{
    confinement::{
        AttemptSeparation, ConfinementMode as SandboxMode, ExecutionConfinement,
        QualificationRecord, authorize,
    },
    harness_config::GeminiAgentHome,
};
use docker_live::*;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

const HARNESS: &str = "gemini";
const MATERIAL: &str = "/aq/private/material";
const OUT: &str = "/aq/work/out";
const HOME: &str = "/aq/agent-home";
const REPO: &str = "/aq/work/repo";
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PROMPT: &str = "The unit test in src/lib.rs fails. Fix the add function in src/lib.rs so that cargo test passes. Edit only src/lib.rs and do not run any commands.";
/// Placeholders substituted in the rendered environment by the runner; the
/// real origin and capability exist only at run time in the sandbox.
const ORIGIN_PLACEHOLDER: &str = "http://127.0.0.1:1";
const CAPABILITY_PLACEHOLDER: &str =
    "aq-gemini-0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    ApiKeyProxy,
    Subscription,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The adapter's own recipe (argv, environment, system settings) for the
/// sandbox paths. The home is installed in a scratch dir only to obtain the
/// settings bytes; paths are then rewritten to the sandbox home.
fn recipe(mode: Mode, model_id: &str) -> (Vec<String>, Vec<(String, String)>, String) {
    let text = |s: &str| Text::new(s).unwrap();
    let artifact = ArtifactRef {
        id: text("i2-live"),
        version: text("1"),
        digest: Digest::of(b"i2-live"),
    };
    let harness = HarnessSettings {
        harness: text(HARNESS),
        cli_version: text(GEMINI_CLI_VERSION),
        adapter: artifact.clone(),
        environment: artifact,
        required_capabilities: [
            "aq.harness.startup-context",
            "aq.harness.container-confinement",
            "aq.harness.stop",
        ]
        .map(|id| Capability {
            id: text(id),
            version: 1,
        })
        .to_vec(),
    };
    let model = ModelSettings {
        provider: text("google"),
        model: text(model_id),
        reasoning: text("medium"),
        max_output_tokens: 8192,
        spending_budget: None,
    };
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    let workspace = Path::new(REPO);
    let separation = AttemptSeparation {
        runner_uid: 1001,
        agent: aq_session_host::users::AgentIdentity {
            uid: 1002,
            gid: 1002,
        },
        credentials_attempt_scoped: true,
    };
    let container = authorize(
        SandboxMode::Container,
        &ExecutionConfinement {
            permit_container: true,
            require_native: false,
        },
        &QualificationRecord::docker_local("aq-rust"),
        &separation,
    )
    .unwrap();
    let auth = match mode {
        Mode::ApiKeyProxy => GeminiAuth::ApiKeyProxy {
            origin: ORIGIN_PLACEHOLDER,
            capability: CAPABILITY_PLACEHOLDER,
        },
        Mode::Subscription => GeminiAuth::Subscription,
    };
    let recipe = GeminiAgentHome::install(
        &container,
        &harness,
        &model,
        GeminiPaths {
            node: Path::new("/opt/aq/node/bin/node"),
            script: Path::new("/opt/aq/harnesses/gemini/bundle/gemini.js"),
            workspace,
            private_home: &home,
        },
        auth,
    )
    .unwrap();
    assert_eq!(recipe.executable, Path::new("/opt/aq/node/bin/node"));
    assert_eq!(recipe.working_directory, workspace);
    let host_home = home.to_str().unwrap();
    let environment = recipe
        .environment
        .into_iter()
        .map(|(k, v)| (k, v.replace(host_home, HOME)))
        .collect();
    (
        recipe.arguments,
        environment,
        String::from_utf8(recipe.system_settings_json).unwrap(),
    )
}

/// The fixed runner. It carries no secret: the credential is read from the
/// delivered material file, which is moved and deleted before work starts.
fn runner(mode: Mode, model_id: &str) -> String {
    let (arguments, environment, settings) = recipe(mode, model_id);
    let argv: Vec<String> = arguments.iter().map(|a| sh_quote(a)).collect();
    let exports: Vec<String> = environment
        .iter()
        .map(|(k, v)| match v.as_str() {
            ORIGIN_PLACEHOLDER => format!("export {k}=\"$origin\""),
            CAPABILITY_PLACEHOLDER => format!("export {k}=\"$capability\""),
            v => format!("export {k}={}", sh_quote(v)),
        })
        .collect();
    let install = match mode {
        Mode::ApiKeyProxy => {
            r#"install -d -m 0700 /aq/runner
cat "$material" > /aq/runner/provider.key
rm -f "$material"
pattern="$(cat /aq/runner/provider.key)"
/opt/aq/bin/aq-worker-test-support gemini-proxy --key-file /aq/runner/provider.key --ready-file /aq/runner/ready.json > "$out/proxy.log" 2>&1 &
proxy=$!
for _ in $(seq 1 60); do [ -s /aq/runner/ready.json ] && break; sleep 0.5; done
[ -s /aq/runner/ready.json ] || fail proxy_not_ready
rm -f /aq/runner/provider.key
origin=$(sed -n 's/.*"origin":"\([^"]*\)".*/\1/p' /aq/runner/ready.json)
capability=$(sed -n 's/.*"capability":"\([^"]*\)".*/\1/p' /aq/runner/ready.json)
[ -n "$origin" ] && [ -n "$capability" ] || fail proxy_ready_file"#
        }
        Mode::Subscription => {
            r#"install -d -m 0700 "$home/.gemini"
cat "$material" > "$home/.gemini/oauth_creds.json"
rm -f "$material"
pattern=$(sed -n 's/.*"refresh_token": *"\([^"]*\)".*/\1/p' "$home/.gemini/oauth_creds.json")
[ -n "$pattern" ] || fail oauth_creds_without_refresh_token"#
        }
    };
    let leak_scope = match mode {
        // The key lives only in the proxy's memory once it has started.
        Mode::ApiKeyProxy => {
            r#"leaks=$(printf '%s' "$pattern" | grep -rlF -f - "$home" /aq /tmp 2>/dev/null | wc -l)"#
        }
        // The refresh token is expected only in the installed login file.
        Mode::Subscription => {
            r#"leaks=$(printf '%s' "$pattern" | grep -rlF -f - "$home" /aq /tmp 2>/dev/null | grep -vx "$home/.gemini/oauth_creds.json" | wc -l)"#
        }
    };
    let stop_proxy = match mode {
        Mode::ApiKeyProxy => "kill -TERM \"$proxy\"; wait \"$proxy\"; proxy_rc=$?",
        Mode::Subscription => "proxy_rc=0",
    };
    format!(
        r#"set -uo pipefail
out={OUT}; mkdir -p "$out"
material={MATERIAL}
home={HOME}
fail() {{ printf '{{"error":"%s"}}\n' "$1" > "$out/result.json"; exit 2; }}
[ -s "$material" ] || fail material_missing
umask 077
# Root-owned 0700 home: Gemini reads system settings only from a root-owned,
# non-group/other-writable file and parent directory.
install -d -m 0700 "$home"
cat > "$home/system-settings.json" <<'JSON'
{settings}
JSON
{install}
material_wiped=false; [ -e "$material" ] || material_wiped=true
export GIT_AUTHOR_NAME=aq-sandbox GIT_AUTHOR_EMAIL=sandbox@aq.invalid
export GIT_COMMITTER_NAME=aq-sandbox GIT_COMMITTER_EMAIL=sandbox@aq.invalid
git init -q --bare -b main /aq/work/publish.git || fail publish_init
repo={REPO}; mkdir -p "$repo/src"; cd "$repo" || fail repo
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
# Direct argv under the bundle Node; only the recipe environment is exported.
printf '%s' {prompt} | (
  for v in $(compgen -e); do unset "$v"; done
  {exports}
  exec timeout 900 /opt/aq/node/bin/node {argv}
) > "$out/gemini.jsonl" 2> "$out/gemini.err"
gemini_rc=$?
check_rc=0; cargo test --offline -q > "$out/check.log" 2>&1 || check_rc=$?
git diff --name-only "$base" > "$out/changed.txt"
publish_rc=0
{{ git commit -qam "fix add" && git push -q /aq/work/publish.git HEAD:refs/heads/main; }} > "$out/publish.log" 2>&1 || publish_rc=$?
head=$(git rev-parse HEAD)
published=$(git --git-dir=/aq/work/publish.git rev-parse --verify -q refs/heads/main || echo none)
recordings=0
for f in "$home"/.gemini/tmp/*/chats/session-*.jsonl; do
  [ -f "$f" ] && cp "$f" "$out/recording.jsonl" && recordings=$((recordings + 1))
done
{stop_proxy}
# Leak scan: the pattern comes from a pipe, never from argv.
{leak_scope}
unset pattern
version=$(HOME=$(mktemp -d) GEMINI_CLI_NO_RELAUNCH=true /opt/aq/bin/gemini --version)
cap_eff=$(awk '/^CapEff:/ {{print $2}}' /proc/self/status)
nnp=$(awk '/^NoNewPrivs:/ {{print $2}}' /proc/self/status)
printf '{{"cli_version":"%s","uid":%s,"cap_eff":"%s","no_new_privs":%s,"material_wiped":%s,"red":%s,"gemini_rc":%s,"check_rc":%s,"publish_rc":%s,"proxy_rc":%s,"base":"%s","head":"%s","published":"%s","secret_files":%s,"recordings":%s}}\n' \
  "$version" "$(id -u)" "$cap_eff" "$nnp" "$material_wiped" "$red" "$gemini_rc" "$check_rc" "$publish_rc" "$proxy_rc" "$base" "$head" "$published" "$leaks" "$recordings" > "$out/result.json"
[ "$gemini_rc" = 0 ] && [ "$check_rc" = 0 ] && [ "$publish_rc" = 0 ]
"#,
        prompt = sh_quote(PROMPT),
        exports = exports.join("\n  "),
        argv = argv.join(" "),
    )
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live Gemini test"))
}

fn model() -> String {
    std::env::var("AQ_GEMINI_LIVE_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".into())
}

#[test]
fn docker_local_offers_container_confinement_only() {
    assert!(require_confinement(ConfinementMode::Container).is_ok());
    assert!(require_confinement(ConfinementMode::Native).is_err());
}

#[test]
fn runner_carries_no_secret_and_wipes_the_material() {
    for mode in [Mode::ApiKeyProxy, Mode::Subscription] {
        let runner = runner(mode, "gemini-2.5-flash");
        // Public configuration: names variables, never a credential value.
        assert!(!runner.contains("AIza") && !runner.contains("ya29.") && !runner.contains("1//"));
        assert!(!runner.contains(CAPABILITY_PLACEHOLDER) && !runner.contains(ORIGIN_PLACEHOLDER));
        let wipe = runner.find("rm -f \"$material\"").unwrap();
        let first_use = runner.find("git init").unwrap();
        assert!(
            wipe < first_use,
            "material must be deleted before work starts"
        );
        assert!(
            runner.contains("grep -rlF -f -"),
            "leak scan pattern via pipe"
        );
        // Direct argv under the bundle Node, headless, no approval prompts.
        assert!(runner.contains("exec timeout 900 /opt/aq/node/bin/node '/opt/aq/harnesses/gemini/bundle/gemini.js' '-p' ''"));
        assert!(runner.contains("'--approval-mode' 'yolo'"));
        assert!(runner.contains("'--model' 'gemini-2.5-flash'"));
        assert!(runner.contains(&format!("export GEMINI_CLI_HOME='{HOME}'")));
        assert!(runner.contains(&format!(
            "export GEMINI_CLI_SYSTEM_SETTINGS_PATH='{HOME}/system-settings.json'"
        )));
    }
    let proxy = runner(Mode::ApiKeyProxy, "gemini-2.5-flash");
    assert!(proxy.contains("export GEMINI_API_KEY=\"$capability\""));
    assert!(proxy.contains("export GOOGLE_GEMINI_BASE_URL=\"$origin\""));
    assert!(proxy.contains(r#""enforcedType": "gemini-api-key""#));
    // The key file is gone once the proxy holds the key in memory.
    assert!(
        proxy.find("rm -f /aq/runner/provider.key").unwrap()
            < proxy.find("exec timeout 900").unwrap()
    );
    let subscription = runner(Mode::Subscription, "gemini-2.5-flash");
    assert!(!subscription.contains("GEMINI_API_KEY"));
    assert!(!subscription.contains("GOOGLE_GEMINI_BASE_URL"));
    assert!(subscription.contains(r#""enforcedType": "oauth-personal""#));
}

struct Run {
    exit: Option<RunnerExit>,
    outputs: std::collections::BTreeMap<&'static str, Vec<u8>>,
    source: Vec<u8>,
    material_gone: bool,
    login_after: Option<Vec<u8>>,
}

async fn run_live(mode: Mode, material: zeroize::Zeroizing<Vec<u8>>) -> Run {
    let socket = PathBuf::from(env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = env("AQ_GEMINI_LIVE_IMAGE");
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let mut config = DockerLocalConfig::new(socket);
    config.templates.push(AllowedTemplate {
        image: image.clone(),
        digest: digest.clone(),
    });
    config.runner_command = Some(vec![
        "/bin/bash".into(),
        "-c".into(),
        runner(mode, &model()),
    ]);
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
        .expect("gemini-rust image present by digest");
    let shared = Shared(Arc::new(Mutex::new(backend)));

    let dir = tempfile::tempdir().unwrap();
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = Controller::open(
        &dir.path().join("controller"),
        Uuid::new_v4(),
        node(&digest, HARNESS, GEMINI_CLI_VERSION),
        shared.clone(),
        Lease {
            token: material,
            deadline: deadline.clone(),
        },
    )
    .unwrap();
    c.register().await.unwrap();
    let request = launch(
        c.identity(),
        &digest,
        HARNESS,
        GEMINI_CLI_VERSION,
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
    let mut outputs = std::collections::BTreeMap::new();
    for name in [
        "result.json",
        "gemini.jsonl",
        "gemini.err",
        "proxy.log",
        "recording.jsonl",
        "red.log",
        "check.log",
        "changed.txt",
        "publish.log",
    ] {
        let bytes = read(&shared, &sandbox, &format!("{OUT}/{name}")).await;
        outputs.insert(name, bytes.unwrap_or_default());
    }
    let source = read(&shared, &sandbox, &format!("{REPO}/src/lib.rs"))
        .await
        .unwrap_or_default();
    let material_gone = read(&shared, &sandbox, MATERIAL).await.is_none();
    // C6b write-back: read the possibly refreshed login before destroy wipes it.
    let login_after = match mode {
        Mode::Subscription => {
            read(
                &shared,
                &sandbox,
                &format!("{HOME}/.gemini/oauth_creds.json"),
            )
            .await
        }
        Mode::ApiKeyProxy => None,
    };

    // Teardown before any outcome assertion, so a failing run still wipes.
    let stopped = c.stop(id).await.unwrap().is_confirmed();
    let disk_after_stop = c.inspect(id).await.unwrap().disk_present;
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "i2-live").unwrap();
    let destroyed = c.destroy(id, &release).await.unwrap().is_confirmed();
    let owned = shared
        .0
        .lock()
        .await
        .list_owned(c.identity().node_id)
        .await
        .unwrap();
    assert!(stopped, "stop confirmed");
    assert!(disk_after_stop, "stop preserves the work volume");
    assert!(destroyed, "destroy confirmed");
    assert!(owned.is_empty(), "teardown left {owned:?}");
    Run {
        exit,
        outputs,
        source,
        material_gone,
        login_after,
    }
}

/// Feed the live recording to the shipped `gemini-recording-jsonl-6814e00`
/// reader; returns the usage records it emits.
fn read_recording(recording: &[u8]) -> usize {
    use aq_observation::{framing::*, lifecycle::*, *};
    let text = std::str::from_utf8(recording).expect("utf8 recording");
    let header: Value = serde_json::from_str(text.lines().next().expect("header")).unwrap();
    let source: Source = serde_json::from_value(serde_json::json!({
        "binding": {"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}},
        "source_id": Uuid::from_u128(8), "generation": 1,
        "native_session_key": header["sessionId"], "format": "gemini_recording6814e00",
        "provider": "google", "account_reference": "i2-live", "registered_session_key": null
    }))
    .expect("gemini source");
    let mut state = Normalizer::new(source, 128).unwrap();
    let mut usage = 0;
    for line in text.lines() {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        let frame = Frame {
            start: 0,
            bytes,
            oversized: false,
        };
        let (next, events) = state.apply(&frame, 100).expect("reader accepts record");
        state = next;
        for event in &events {
            event.validate().unwrap();
            usage += matches!(event.body.kind, ObservationKind::Usage { .. }) as usize;
        }
    }
    usage
}

fn assert_cycle(run: &Run, secret: &[u8], evidence: Option<&Path>) -> Value {
    if let Some(dir) = evidence {
        std::fs::create_dir_all(dir).unwrap();
        for (name, bytes) in &run.outputs {
            if !contains(bytes, secret) {
                std::fs::write(dir.join(name), bytes).unwrap();
            }
        }
        std::fs::write(dir.join("lib.rs"), &run.source).unwrap();
    }
    for (name, bytes) in &run.outputs {
        assert!(!contains(bytes, secret), "{name} carries the credential");
    }
    let result: Value = serde_json::from_slice(&run.outputs["result.json"]).unwrap_or(Value::Null);
    eprintln!("runner exit {:?}: {result}", run.exit);
    assert_eq!(run.exit, Some(RunnerExit::Exited(0)), "result: {result}");
    assert_eq!(result["cli_version"], GEMINI_CLI_VERSION);
    assert_eq!(result["cap_eff"], "0000000000000000", "no capabilities");
    assert_eq!(result["no_new_privs"], 1);
    assert_eq!(result["material_wiped"], true);
    assert!(
        run.material_gone,
        "material file absent after the lease is read"
    );
    assert_eq!(result["red"], true, "baseline test fails before the edit");
    assert_eq!(result["check_rc"], 0);
    assert_eq!(result["publish_rc"], 0);
    assert_eq!(result["published"], result["head"]);
    assert_ne!(result["head"], result["base"]);
    assert_eq!(
        String::from_utf8_lossy(&run.outputs["changed.txt"]).trim(),
        "src/lib.rs"
    );
    assert!(String::from_utf8_lossy(&run.source).contains("a + b"));
    assert_eq!(result["secret_files"], 0, "credential copied to disk");
    assert_eq!(result["recordings"], 1, "one native recording");
    let stream = String::from_utf8_lossy(&run.outputs["gemini.jsonl"]);
    assert!(
        stream
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|v| v["type"] == "result" && v["status"] == "success"),
        "Gemini reported success"
    );
    assert!(
        read_recording(&run.outputs["recording.jsonl"]) >= 1,
        "reader emits native usage from the live recording"
    );
    result
}

#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the gemini-rust image and a Gemini API key"]
async fn live_gemini_api_key_proxy_edit_check_publish_cycle() {
    let key = read_token(Path::new(&env("AQ_GEMINI_LIVE_KEY_FILE")));
    let evidence =
        std::env::var_os("AQ_GEMINI_LIVE_EVIDENCE_DIR").map(|d| PathBuf::from(d).join("api-key"));
    let run = run_live(Mode::ApiKeyProxy, key.clone()).await;
    let result = assert_cycle(&run, &key, evidence.as_deref());
    assert_eq!(result["proxy_rc"], 0, "proxy stopped cleanly");
}

#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the gemini-rust image and an enrolled Google login"]
async fn live_gemini_subscription_edit_check_publish_cycle_with_write_back() {
    let path = PathBuf::from(env("AQ_GEMINI_LIVE_OAUTH_CREDS_FILE"));
    let login = read_token(&path);
    let leased: Value = serde_json::from_slice(&login).expect("oauth_creds.json");
    let refresh = leased["refresh_token"]
        .as_str()
        .expect("enrolled login carries a refresh token")
        .as_bytes()
        .to_vec();
    let evidence = std::env::var_os("AQ_GEMINI_LIVE_EVIDENCE_DIR")
        .map(|d| PathBuf::from(d).join("subscription"));
    let run = run_live(Mode::Subscription, login.clone()).await;
    // Write back first: a refreshed login must not be lost to a failed assertion.
    let after = run
        .login_after
        .clone()
        .expect("login file present after the run");
    let refreshed: Value = serde_json::from_slice(&after).expect("refreshed oauth_creds.json");
    assert!(refreshed["refresh_token"].is_string());
    if after.trim_ascii() != login.as_slice() {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let staged = path.with_extension("aq-write-back");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .unwrap();
        file.write_all(&after).unwrap();
        file.sync_all().unwrap();
        std::fs::rename(&staged, &path).unwrap();
    }
    assert_cycle(&run, &refresh, evidence.as_deref());
}

#[test]
fn live_reader_path_reads_the_qualified_cli_recording() {
    let fixture =
        include_bytes!("../../aq-observation/tests/fixtures/gemini-cli-0.67.0-recording.jsonl");
    assert_eq!(read_recording(fixture), 2, "one usage per model turn");
}
