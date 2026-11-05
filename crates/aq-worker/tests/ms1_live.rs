//! MS1 (`stark-zenith-16.2`) node-level live checks: a real Claude Code run in
//! a docker-local sandbox, driven by the B5 node controller, is cancelled
//! mid-run, and separately survives a node-controller restart without a
//! relaunch. Live and `#[ignore]`d; see aq-server `docs/implementation/MS1.md`.
//!
//! These checks have no aq-server in the loop. The production controller has no
//! B2 registration/issuance transport yet, so cancellation is an operator
//! `stop` and restart recovery cannot reach server re-adoption. They prove the
//! sandbox side only: stop proof, preserved disk, no duplicate create.
//!
//! Environment is the I4a set (`claude_live.rs`): `AQ_DOCKER_LOCAL_SOCKET`,
//! `AQ_CLAUDE_LIVE_IMAGE`, `AQ_CLAUDE_LIVE_TOKEN_FILE`, optional
//! `AQ_CLAUDE_LIVE_CLI_VERSION`, `AQ_DOCKER_LOCAL_SHARED_ROOT_FS` and
//! `AQ_CLAUDE_LIVE_EVIDENCE_DIR` (one subdirectory per test).
#[path = "support/docker_live.rs"]
mod docker_live;
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use docker_live::*;
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
use zeroize::Zeroizing;

const DEFAULT_CLI_VERSION: &str = "2.1.318";
const HARNESS: &str = "claude-code";
const MATERIAL: &str = "/aq/private/material";
const OUT: &str = "/aq/work/out";
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const FIRST_TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// A deliberately long Claude run (twenty sequential writes), so the test can
/// interrupt it after Claude has produced real model output. Like the I4a
/// runner it carries no secret and deletes the material before any work.
const LONG_RUNNER: &str = r#"set -uo pipefail
out=/aq/work/out; mkdir -p "$out"
material=/aq/private/material
[ -s "$material" ] || { echo material_missing > "$out/error"; exit 2; }
CLAUDE_CODE_OAUTH_TOKEN="$(cat "$material")"; export CLAUDE_CODE_OAUTH_TOKEN
rm -f "$material"
[ -e "$material" ] || touch "$out/material_wiped"
repo=/aq/work/repo; mkdir -p "$repo/notes"; cd "$repo" || exit 2
export GIT_AUTHOR_NAME=aq-sandbox GIT_AUTHOR_EMAIL=sandbox@aq.invalid
export GIT_COMMITTER_NAME=aq-sandbox GIT_COMMITTER_EMAIL=sandbox@aq.invalid
git init -q -b main && git commit -q --allow-empty -m baseline
date -u +%s > "$out/claude.started"
claude -p "Create twenty files notes/01.txt through notes/20.txt, one file per Write tool call and in order. Each file holds one short original sentence about its number. Do not run any commands." \
  --permission-mode acceptEdits --allowedTools Read,Write --max-turns 60 \
  --output-format stream-json --verbose > "$out/claude.jsonl" 2> "$out/claude.err"
echo $? > "$out/claude.rc"
"#;

struct Live {
    shared: Shared,
    digest: TemplateDigest,
    cli: String,
    token: Zeroizing<Vec<u8>>,
    evidence: Option<PathBuf>,
    _dir: tempfile::TempDir,
    root: PathBuf,
    node_id: Uuid,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the MS1 live test"))
}

async fn live(name: &str) -> Live {
    live_with(name, LONG_RUNNER).await
}

async fn live_with(name: &str, runner: &str) -> Live {
    let socket = PathBuf::from(env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = env("AQ_CLAUDE_LIVE_IMAGE");
    let cli =
        std::env::var("AQ_CLAUDE_LIVE_CLI_VERSION").unwrap_or_else(|_| DEFAULT_CLI_VERSION.into());
    let token = read_token(Path::new(&env("AQ_CLAUDE_LIVE_TOKEN_FILE")));
    let evidence =
        std::env::var_os("AQ_CLAUDE_LIVE_EVIDENCE_DIR").map(|d| PathBuf::from(d).join(name));
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let mut config = DockerLocalConfig::new(socket);
    config.templates.push(AllowedTemplate {
        image,
        digest: digest.clone(),
    });
    config.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), runner.into()]);
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
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("controller");
    Live {
        shared: Shared(Arc::new(Mutex::new(backend))),
        digest,
        cli,
        token,
        evidence,
        _dir: dir,
        root,
        node_id: Uuid::new_v4(),
    }
}

impl Live {
    fn controller(&self, deadline: &Arc<AtomicU64>) -> Controller<Shared, Lease> {
        Controller::open(
            &self.root,
            self.node_id,
            node(&self.digest, HARNESS, &self.cli),
            self.shared.clone(),
            Lease {
                token: self.token.clone(),
                deadline: deadline.clone(),
            },
        )
        .unwrap()
    }

    /// open → register → reserve → accept → launch; returns the attempt.
    async fn start(&self, c: &mut Controller<Shared, Lease>, deadline: &Arc<AtomicU64>) -> Uuid {
        c.register().await.unwrap();
        let request = launch(c.identity(), &self.digest, HARNESS, &self.cli, RUN_TIMEOUT);
        deadline.store(request.hard_deadline_ms, Ordering::SeqCst);
        let id = c.reserve(request).unwrap();
        c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
        c.launch(id, &()).await.expect("controller launch");
        id
    }

    /// Wait until the stream-json transcript holds a model turn, so the
    /// interruption provably lands on a running Claude session.
    async fn wait_for_first_turn(&self, sandbox: &SandboxIdentity) -> Duration {
        let started = Instant::now();
        loop {
            let transcript = read(&self.shared, sandbox, &format!("{OUT}/claude.jsonl"))
                .await
                .unwrap_or_default();
            if String::from_utf8_lossy(&transcript).contains(r#""type":"assistant""#) {
                return started.elapsed();
            }
            assert!(
                started.elapsed() < FIRST_TURN_TIMEOUT,
                "no Claude turn within {FIRST_TURN_TIMEOUT:?}"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Read back the outputs from the preserved volume and keep the evidence.
    async fn collect(&self, sandbox: &SandboxIdentity) -> Vec<(String, Vec<u8>)> {
        let mut outputs = Vec::new();
        for name in [
            "claude.jsonl",
            "claude.err",
            "claude.rc",
            "claude.started",
            "material_wiped",
        ] {
            let bytes = read(&self.shared, sandbox, &format!("{OUT}/{name}")).await;
            if let Some(bytes) = bytes {
                outputs.push((name.to_string(), bytes));
            }
        }
        if let Some(dir) = &self.evidence {
            std::fs::create_dir_all(dir).unwrap();
            for (name, bytes) in &outputs {
                if !contains(bytes, &self.token) {
                    std::fs::write(dir.join(name), bytes).unwrap();
                }
            }
        }
        for (name, bytes) in &outputs {
            assert!(!contains(bytes, &self.token), "{name} carries the token");
        }
        outputs
    }

    fn record(&self, name: &str, body: serde_json::Value) {
        eprintln!("{name}: {body}");
        if let Some(dir) = &self.evidence {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(name), serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        }
    }
}

fn notes_written(transcript: &[u8]) -> usize {
    String::from_utf8_lossy(transcript)
        .matches(r#""name":"Write""#)
        .count()
}

#[test]
fn long_runner_carries_no_secret_and_wipes_the_material_first() {
    assert!(!LONG_RUNNER.contains("sk-ant-"));
    let wipe = LONG_RUNNER.find("rm -f \"$material\"").unwrap();
    assert!(wipe < LONG_RUNNER.find("git init").unwrap());
    assert!(wipe < LONG_RUNNER.find("claude -p").unwrap());
    assert!(!LONG_RUNNER.contains("--dangerously-skip-permissions"));
}

/// MS1 step 3, sandbox side: an operator stop interrupts a running Claude
/// session, the backend proves no writer remains, the work volume survives
/// until an explicit destroy, and the run never reaches its own exit.
#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the claude-rust image and the owner's token"]
async fn ms1_cancel_mid_claude_run_stops_with_proof() {
    let live = live("cancel").await;
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = live.controller(&deadline);
    let id = live.start(&mut c, &deadline).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let first_turn = live.wait_for_first_turn(&sandbox).await;

    let stop_started = Instant::now();
    let stop = c.stop(id).await.unwrap();
    let stop_elapsed = stop_started.elapsed();
    let proof = match &stop {
        Confirmation::Confirmed(proof) => Some(proof.clone()),
        _ => None,
    };
    let phase_after_stop = c.state().attempts[&id].phase;
    let after = c.inspect(id).await.unwrap();
    let exit = live.shared.0.lock().await.runner_exit(&sandbox).await.ok();
    let outputs = live.collect(&sandbox).await;
    let transcript = outputs
        .iter()
        .find(|(n, _)| n == "claude.jsonl")
        .map(|(_, b)| b.clone())
        .unwrap_or_default();
    let finished = outputs.iter().any(|(n, _)| n == "claude.rc");
    let material_gone = read(&live.shared, &sandbox, MATERIAL).await.is_none();

    let release = DestroyAuthorization::operator_released(sandbox.clone(), "ms1-cancel").unwrap();
    let destroyed = c.destroy(id, &release).await.unwrap().is_confirmed();
    let owned = live
        .shared
        .0
        .lock()
        .await
        .list_owned(c.identity().node_id)
        .await
        .unwrap();
    live.record(
        "cancel.json",
        serde_json::json!({
            "attempt_id": id,
            "sandbox_backend_id": sandbox.backend_id,
            "template_digest": live.digest.as_str(),
            "first_claude_turn_after_ms": first_turn.as_millis() as u64,
            "stop_confirm_ms": stop_elapsed.as_millis() as u64,
            "stop_proof": proof.as_ref().map(|p| format!("{:?}", p.kind())),
            "phase_after_stop": format!("{phase_after_stop:?}"),
            "disk_present_after_stop": after.disk_present,
            "runner_exit": format!("{exit:?}"),
            "claude_reached_its_own_exit": finished,
            "write_calls_before_stop": notes_written(&transcript),
            "material_absent": material_gone,
            "destroyed": destroyed,
            "owned_after_destroy": owned.len(),
        }),
    );
    assert!(
        proof.is_some(),
        "stop confirmed with a stopped-writer proof"
    );
    assert_eq!(proof.unwrap().kind(), WriterProofKind::StoppedNoProcesses);
    assert!(after.disk_present, "stop preserves the work volume");
    assert!(!finished, "Claude must not have completed before the stop");
    assert!(notes_written(&transcript) < 20, "run interrupted mid-way");
    assert!(material_gone, "material wiped by the runner");
    assert!(destroyed, "destroy confirmed");
    assert!(owned.is_empty(), "teardown left {owned:?}");
}

/// MS1 step 5, node side (Review Focus 4): the node controller process goes
/// away while Claude runs in the sandbox. A new controller incarnation on the
/// same journal reconciles the live sandbox. Without a server re-adoption
/// answer it must never create or start a second sandbox; it stops and
/// preserves the old-boot attempt instead.
#[tokio::test]
#[ignore = "live: needs the sandbox daemon, the claude-rust image and the owner's token"]
async fn ms1_node_restart_mid_claude_run_preserves_without_relaunch() {
    let live = live("node-restart").await;
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = live.controller(&deadline);
    let id = live.start(&mut c, &deadline).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let first_boot = c.identity().boot_id;
    let first_turn = live.wait_for_first_turn(&sandbox).await;
    let before = live
        .shared
        .0
        .lock()
        .await
        .list_owned(live.node_id)
        .await
        .unwrap();
    // The controller process ends abruptly: no stop, no drain.
    drop(c);

    let restart_started = Instant::now();
    let mut c = live.controller(&deadline);
    let second_boot = c.identity().boot_id;
    c.reconcile_at(now_ms()).await.unwrap();
    let reconcile_elapsed = restart_started.elapsed();
    let attempt = c.state().attempts[&id].clone();
    let after = live
        .shared
        .0
        .lock()
        .await
        .list_owned(live.node_id)
        .await
        .unwrap();
    let inspect = c.inspect(id).await.unwrap();
    let outputs = live.collect(&sandbox).await;
    let transcript = outputs
        .iter()
        .find(|(n, _)| n == "claude.jsonl")
        .map(|(_, b)| b.clone())
        .unwrap_or_default();
    let finished = outputs.iter().any(|(n, _)| n == "claude.rc");

    // Teardown before the outcome assertions.
    if attempt.stopped.is_none() {
        let _ = c.stop(id).await;
    }
    // Preserve-before-destroy: the journaled cleanup grace must elapse before
    // even an operator release may delete the preserved disk.
    let release =
        DestroyAuthorization::operator_released(sandbox.clone(), "ms1-node-restart").unwrap();
    let grace_started = Instant::now();
    let mut refused_during_grace = 0u32;
    let destroyed = loop {
        match c.destroy(id, &release).await {
            Ok(confirmation) => break confirmation.is_confirmed(),
            Err(e)
                if format!("{e:#}").contains("grace")
                    && grace_started.elapsed() < Duration::from_secs(120) =>
            {
                refused_during_grace += 1;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => panic!("destroy failed: {e:#}"),
        }
    };
    let destroy_waited = grace_started.elapsed();
    let owned = live
        .shared
        .0
        .lock()
        .await
        .list_owned(live.node_id)
        .await
        .unwrap();
    live.record(
        "node-restart.json",
        serde_json::json!({
            "attempt_id": id,
            "sandbox_backend_id": sandbox.backend_id,
            "template_digest": live.digest.as_str(),
            "first_boot_id": first_boot,
            "second_boot_id": second_boot,
            "first_claude_turn_after_ms": first_turn.as_millis() as u64,
            "reopen_and_reconcile_ms": reconcile_elapsed.as_millis() as u64,
            "owned_before_restart": before.len(),
            "owned_after_reconcile": after.len(),
            "same_backend_id_after": after.iter().all(|e| e.sandbox.backend_id == sandbox.backend_id),
            "phase_after_reconcile": format!("{:?}", attempt.phase),
            "cleanup_after_reconcile": format!("{:?}", attempt.cleanup.as_ref().map(|c| format!("{c:?}"))),
            "stop_proof": attempt.stopped.as_ref().map(|p| format!("{:?}", p.kind())),
            "disk_present_after_reconcile": inspect.disk_present,
            "claude_reached_its_own_exit": finished,
            "write_calls_before_stop": notes_written(&transcript),
            "destroy_refused_during_grace": refused_during_grace,
            "destroy_waited_ms": destroy_waited.as_millis() as u64,
            "destroyed": destroyed,
            "owned_after_destroy": owned.len(),
        }),
    );
    assert_ne!(first_boot, second_boot, "restart is a new boot incarnation");
    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1, "no second sandbox was created");
    assert!(
        after
            .iter()
            .all(|e| e.sandbox.backend_id == sandbox.backend_id)
    );
    assert!(
        attempt.stopped.is_some(),
        "an old-boot attempt without server re-adoption is stopped"
    );
    assert!(inspect.disk_present, "stop-and-preserve keeps the volume");
    assert!(destroyed, "destroy confirmed");
    assert!(owned.is_empty(), "teardown left {owned:?}");
}

/// MS1 step 6 helper (Review Focus 1), for the owner's manual Windows-suspend
/// drill: run the long Claude session to its own end while the owner suspends
/// and resumes Windows. The test logs every poll; a wall-clock gap above 20 s
/// between polls is recorded as a suspend window. It then records whether the
/// sandbox survived, whether Claude finished all twenty writes, and tears down.
/// Without a suspend it is simply a long, uninterrupted live run.
#[tokio::test]
#[ignore = "live, manual: the owner suspends Windows while this runs"]
async fn ms1_suspend_drill_long_claude_run() {
    // About five times the cancel run (~3 min), so the owner has time to suspend.
    let runner = LONG_RUNNER
        .replace(
            "twenty files notes/01.txt through notes/20.txt",
            "one hundred files notes/001.txt through notes/100.txt",
        )
        .replace("--max-turns 60", "--max-turns 250");
    assert_ne!(runner, LONG_RUNNER);
    let live = live_with("suspend-drill", &runner).await;
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = live.controller(&deadline);
    let id = live.start(&mut c, &deadline).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    eprintln!(
        "drill: sandbox {} running; suspend Windows now",
        sandbox.backend_id
    );
    let started = Instant::now();
    let mut last = (now_ms(), Instant::now());
    let mut gaps = Vec::new();
    let mut polls = Vec::new();
    let exit = loop {
        let exit = live.shared.0.lock().await.runner_exit(&sandbox).await.ok();
        let wall = now_ms();
        let gap_ms = wall.saturating_sub(last.0);
        if gap_ms > 20_000 {
            gaps.push(serde_json::json!({
                "from_utc_ms": last.0,
                "to_utc_ms": wall,
                "wall_gap_ms": gap_ms,
                "monotonic_gap_ms": last.1.elapsed().as_millis() as u64,
            }));
            eprintln!("drill: wall-clock gap of {gap_ms} ms detected (suspend/resume)");
        }
        let transcript = read(&live.shared, &sandbox, &format!("{OUT}/claude.jsonl"))
            .await
            .unwrap_or_default();
        polls.push(serde_json::json!([
            wall,
            transcript.len(),
            notes_written(&transcript)
        ]));
        last = (wall, Instant::now());
        if exit.is_some() || started.elapsed() > RUN_TIMEOUT {
            break exit;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let inspect = c.inspect(id).await.ok();
    let outputs = live.collect(&sandbox).await;
    let transcript = outputs
        .iter()
        .find(|(n, _)| n == "claude.jsonl")
        .map(|(_, b)| b.clone())
        .unwrap_or_default();
    let rc = outputs
        .iter()
        .find(|(n, _)| n == "claude.rc")
        .map(|(_, b)| String::from_utf8_lossy(b).trim().to_string());
    let stopped = c.stop(id).await.map(|s| s.is_confirmed()).unwrap_or(false);
    let release =
        DestroyAuthorization::operator_released(sandbox.clone(), "ms1-suspend-drill").unwrap();
    let destroyed = c
        .destroy(id, &release)
        .await
        .map(|d| d.is_confirmed())
        .unwrap_or(false);
    live.record(
        "suspend-drill.json",
        serde_json::json!({
            "attempt_id": id,
            "sandbox_backend_id": sandbox.backend_id,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "suspend_windows": gaps,
            "runner_exit": format!("{exit:?}"),
            "claude_rc": rc,
            "write_calls": notes_written(&transcript),
            "disk_present_at_end": inspect.map(|i| i.disk_present),
            "stopped": stopped,
            "destroyed": destroyed,
            "polls_utc_ms_bytes_writes": polls,
        }),
    );
    assert!(stopped && destroyed, "teardown");
}
