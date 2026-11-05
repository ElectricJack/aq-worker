//! I1 (after I4a): Claude Code CLI qualification with the owner's Claude
//! subscription login in a real docker-local sandbox, driven by the B5a node
//! controller. The sandbox runs the C6a container recipe itself and the H1
//! reader qualifies its native transcript. Live and `#[ignore]`d; see
//! `docs/implementation/I1.md` (and I4a for the original cycle).
//!
//! Environment:
//! - `AQ_DOCKER_LOCAL_SOCKET`: the dedicated sandbox daemon's API socket.
//! - `AQ_CLAUDE_LIVE_IMAGE`: `repo@sha256:...` of the derived claude-rust image,
//!   already present in that daemon (pulled by digest).
//! - `AQ_CLAUDE_LIVE_TOKEN_FILE`: the owner's 0600 `claude setup-token` file.
//! - `AQ_CLAUDE_LIVE_CLI_VERSION` (default 2.1.318), `AQ_DOCKER_LOCAL_SHARED_ROOT_FS`,
//!   `AQ_CLAUDE_LIVE_EVIDENCE_DIR` (copies the read-back outputs there),
//!   `AQ_CLAUDE_LIVE_MODEL` (default `sonnet`).
//!
//! The token reaches the sandbox only as controller-delivered private
//! material: never argv, labels, create env or logs. The runner moves it into
//! `CLAUDE_CODE_OAUTH_TOKEN`, deletes the file and never writes it back.
#[path = "support/docker_live.rs"]
mod docker_live;
use aq_session_protocol::{ArtifactRef, Capability, Digest, HarnessSettings, ModelSettings, Text};
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use aq_worker::native::adapter::claude::{CLAUDE_CLI_VERSION, ClaudePaths, ClaudeRecipe};
use aq_worker::native::runner::sandbox::{
    confinement::{
        self as sandbox_confinement, AttemptSeparation, ExecutionConfinement, QualificationRecord,
        authorize,
    },
    harness_config::ClaudeAgentHome,
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

const DEFAULT_CLI_VERSION: &str = "2.1.318";
const HARNESS: &str = "claude-code";
const MATERIAL: &str = "/aq/private/material";
const OUT: &str = "/aq/work/out";
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Fixed runner prologue. It carries no secret: the token is read from the
/// delivered material file, which is deleted before anything else runs.
const RUNNER_SETUP: &str = r#"set -uo pipefail
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
"#;

/// Fixed runner epilogue: check, publish, leak scan and the result record.
const RUNNER_FINISH: &str = r#"cd /aq/work/repo || fail repo
check_rc=0; cargo test --offline -q > "$out/check.log" 2>&1 || check_rc=$?
git diff --name-only "$base" > "$out/changed.txt"
publish_rc=0
{ git commit -qam "fix add" && git push -q /aq/work/publish.git HEAD:refs/heads/main; } > "$out/publish.log" 2>&1 || publish_rc=$?
head=$(git rev-parse HEAD)
published=$(git --git-dir=/aq/work/publish.git rev-parse refs/heads/main 2>/dev/null || echo none)
git --git-dir=/aq/work/publish.git log --format='%H %s' -n 2 refs/heads/main > "$out/published.txt" 2>&1
# Leak scan: the pattern comes from a pipe, never from argv.
leaks=$(printenv CLAUDE_CODE_OAUTH_TOKEN | grep -rlF -f - "$HOME" /aq /tmp 2>/dev/null | wc -l)
credentials=false
for f in "$HOME/.claude/.credentials.json" "$agent_home/.credentials.json"; do
  [ -e "$f" ] && credentials=true
done
unset CLAUDE_CODE_OAUTH_TOKEN
version=$(claude --version)
cap_eff=$(awk '/^CapEff:/ {print $2}' /proc/self/status)
nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)
printf '{"cli_version":"%s","uid":%s,"cap_eff":"%s","no_new_privs":%s,"material_wiped":%s,"red":%s,"claude_rc":%s,"check_rc":%s,"publish_rc":%s,"base":"%s","head":"%s","published":"%s","token_files":%s,"credentials_file":%s,"native_transcript":%s}\n' \
  "$version" "$(id -u)" "$cap_eff" "$nnp" "$material_wiped" "$red" "$claude_rc" "$check_rc" "$publish_rc" "$base" "$head" "$published" "$leaks" "$credentials" "$native_transcript" > "$out/result.json"
[ "$claude_rc" = 0 ] && [ "$check_rc" = 0 ] && [ "$publish_rc" = 0 ] && [ "$native_transcript" = true ]
"#;

const PROMPT: &str = "The unit test in src/lib.rs fails. Fix the add function in src/lib.rs so that cargo test passes. Edit only src/lib.rs and do not run any commands.";
const WORKSPACE: &str = "/aq/work/repo";
const AGENT_HOME: &str = "/aq/work/claude-home";
const MAX_TURNS: u32 = 12;

/// The C6a container-mode recipe for in-sandbox paths, authorized exactly as
/// the runner would be on docker-local. Nothing is written on this host.
fn planned_recipe(model: &str, session: Uuid) -> ClaudeRecipe {
    let text = |s: &str| Text::new(s).unwrap();
    let artifact = ArtifactRef {
        id: text("i1-live"),
        version: text("1"),
        digest: Digest::of(b"i1-live"),
    };
    let harness = HarnessSettings {
        harness: text("claude"),
        cli_version: text(CLAUDE_CLI_VERSION),
        adapter: artifact.clone(),
        environment: artifact,
        required_capabilities: [
            "aq.harness.startup-context",
            aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY,
            "aq.harness.stop",
        ]
        .map(|id| Capability {
            id: text(id),
            version: 1,
        })
        .to_vec(),
    };
    let model = ModelSettings {
        provider: text("anthropic"),
        model: text(model),
        reasoning: text("medium"),
        max_output_tokens: 4096,
        spending_budget: None,
    };
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
    ClaudeAgentHome::plan(
        &authorized,
        &harness,
        &model,
        ClaudePaths {
            executable: Path::new("/opt/aq/bin/claude"),
            workspace: Path::new(WORKSPACE),
            private_home: Path::new(AGENT_HOME),
            session,
        },
    )
    .unwrap()
}

/// POSIX single-quoting.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The runner: fixed prologue, the rendered recipe, fixed epilogue. The
/// recipe's settings land in its private home; the CLI runs with only the
/// recipe environment plus the inherited token (never an argv assignment) and
/// the test-only `IS_SANDBOX=1` (in-container root, see I1.md); the prompt
/// arrives on stdin.
fn runner(recipe: &ClaudeRecipe) -> String {
    let settings = String::from_utf8(recipe.settings_json.clone()).unwrap();
    assert!(!settings.contains("AQ_SETTINGS"));
    let exports: String = recipe
        .environment
        .iter()
        .map(|(k, v)| format!("  export {k}={}\n", quote(v)))
        .collect();
    let args: Vec<String> = recipe.arguments.iter().map(|a| quote(a)).collect();
    let home = recipe.environment["CLAUDE_CONFIG_DIR"].as_str();
    format!(
        r#"{RUNNER_SETUP}agent_home={home}
mkdir -p -m 700 "$agent_home" || fail agent_home
cat > "$agent_home/settings.json" <<'AQ_SETTINGS'
{settings}
AQ_SETTINGS
chmod 600 "$agent_home/settings.json"
cd {workdir} || fail workdir
(
  for v in $(compgen -e); do
    [ "$v" = CLAUDE_CODE_OAUTH_TOKEN ] || unset "$v" 2>/dev/null
  done
{exports}  export IS_SANDBOX=1
  exec timeout 900 {exe} {args} --max-turns {MAX_TURNS}
) > "$out/claude.jsonl" 2> "$out/claude.err" <<'AQ_PROMPT'
{PROMPT}
AQ_PROMPT
claude_rc=$?
native_transcript=false
cp {transcript} "$out/session.jsonl" 2>/dev/null && native_transcript=true
find "$agent_home" -type f -printf '%P\n' | sort > "$out/home-files.txt"
{RUNNER_FINISH}"#,
        home = quote(home),
        workdir = quote(recipe.working_directory.to_str().unwrap()),
        exe = quote(recipe.executable.to_str().unwrap()),
        args = args.join(" "),
        transcript = quote(recipe.transcript.to_str().unwrap()),
    )
}

/// The reference normalizer over the native transcript the pinned CLI wrote.
/// Reference-normalizes a native transcript. Returns the facts, the gaps not
/// explained by the 64 KiB frame bound, and the number of bound gaps.
fn normalize(
    session: Uuid,
    bytes: &[u8],
) -> (
    Vec<aq_observation::lifecycle::ObservationRecord>,
    Vec<String>,
    usize,
) {
    use aq_observation::lifecycle::*;
    let source = serde_json::from_value(serde_json::json!({
        "binding": {
            "project_id": Uuid::from_u128(1),
            "task_id": Uuid::from_u128(2),
            "lineage_id": Uuid::from_u128(3),
            "attempt_id": Uuid::from_u128(4),
            "attempt_generation": 1,
            "session": serde_json::to_value(SessionIdentity::Remote {
                node_id: Uuid::from_u128(5),
                boot_id: Uuid::from_u128(6),
                session_id: Uuid::from_u128(7),
                attempt_id: Uuid::from_u128(4),
                assignment_generation: 1,
            }).unwrap(),
        },
        "source_id": Uuid::from_u128(8),
        "generation": 1,
        "native_session_key": session.to_string(),
        "format": serde_json::to_value(aq_observation::Format::ClaudeV1Jsonl9a9b8e8).unwrap(),
        "provider": "anthropic",
        "account_reference": "logical-account",
        "registered_session_key": null,
    }))
    .unwrap();
    let mut normalizer = aq_observation::Normalizer::new(source, 4096).unwrap();
    let frames = aq_observation::framing::frame(bytes, 0, false).unwrap();
    let (mut events, mut unexplained, mut bound) = (vec![], vec![], 0);
    for frame in frames.records {
        let (next, facts) = normalizer.apply(&frame, 100).unwrap();
        normalizer = next;
        for fact in &facts {
            match &fact.body.kind {
                ObservationKind::Gap {
                    reason: GapReason::Malformed,
                    ..
                } if frame.oversized => bound += 1,
                ObservationKind::Gap { .. } => unexplained.push(format!("{:?}", fact.body.kind)),
                _ => {}
            }
        }
        events.extend(facts);
    }
    (events, unexplained, bound)
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live Claude test"))
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
    // The runner is public configuration: it may name the env variable but
    // never carries a token value, and it deletes the material first.
    let session = Uuid::new_v4();
    let recipe = planned_recipe("sonnet", session);
    let runner = runner(&recipe);
    assert!(!runner.contains("sk-ant-"));
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
    assert!(!runner.contains("--dangerously-skip-permissions"));
    // The token is inherited, never an argv assignment (`env K=...`).
    assert!(!runner.contains("CLAUDE_CODE_OAUTH_TOKEN=\"$CLAUDE"));
    assert_eq!(runner.matches("CLAUDE_CODE_OAUTH_TOKEN=").count(), 1);
    // The prompt is stdin, not argv; the recipe never sets IS_SANDBOX.
    assert!(!recipe.arguments.iter().any(|a| a.contains("cargo test")));
    assert!(!recipe.environment.contains_key("IS_SANDBOX"));
    // The real recipe: its pinned session names the transcript it copies.
    assert!(runner.contains(&format!("'--session-id' '{session}'")));
    assert!(runner.contains(&format!(
        "cp '/aq/work/claude-home/projects/-aq-work-repo/{session}.jsonl'"
    )));
    assert!(runner.contains("'--permission-mode' 'bypassPermissions'"));
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
    let model = std::env::var("AQ_CLAUDE_LIVE_MODEL").unwrap_or_else(|_| "sonnet".into());
    let session = Uuid::new_v4();
    let recipe = planned_recipe(&model, session);

    let mut config = DockerLocalConfig::new(socket);
    config.templates.push(AllowedTemplate {
        image: image.clone(),
        digest: digest.clone(),
    });
    config.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), runner(&recipe)]);
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
        node(&digest, HARNESS, &cli),
        shared.clone(),
        Lease {
            token: token.clone(),
            deadline: deadline.clone(),
        },
    )
    .unwrap();
    c.register().await.unwrap();
    let request = launch(c.identity(), &digest, HARNESS, &cli, RUN_TIMEOUT);
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
        "session.jsonl",
        "home-files.txt",
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
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "i1-live").unwrap();
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
    let stream = String::from_utf8_lossy(&outputs["claude.jsonl"]);
    assert!(
        stream
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|v| v["type"] == "result" && v["is_error"] == false),
        "Claude reported success"
    );
    // The native transcript at the recipe's exact path, read by the
    // reference normalizer: per-call usage, the Edit tool cycle and the turn
    // boundary. The only tolerated gaps are the explicit 64 KiB frame-bound
    // ones: 2.1.318 writes `prompt_snapshot` attachments (tool schemas, no
    // facts) beyond the bound (I1.md, limits).
    assert_eq!(
        result["native_transcript"], true,
        "transcript at the recipe path"
    );
    use aq_observation::lifecycle::ObservationKind;
    let (events, gaps, bound) = normalize(session, &outputs["session.jsonl"]);
    eprintln!("frame-bound gaps: {bound}");
    assert!(gaps.is_empty(), "reader gaps: {gaps:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.body.kind, ObservationKind::Usage { .. }))
    );
    let edit = events
        .iter()
        .find_map(|e| match &e.body.kind {
            ObservationKind::ToolStarted { call_id, tool_name } if tool_name.as_str() == "Edit" => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .expect("Edit tool started");
    assert!(events.iter().any(|e| matches!(
        &e.body.kind,
        ObservationKind::ToolResult { call_id, .. } if call_id == &edit
    )));
    assert!(events.iter().any(|e| matches!(
        e.body.kind,
        ObservationKind::TurnBoundary { in_turn: false }
    )));
}
