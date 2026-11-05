//! calm-delta-18.9.4: live native-confinement probe per template × backend.
//! Opt-in qualification only (`#[ignore]`); see
//! `docs/implementation/calm-delta-18.9.4.md`.
//!
//! For each template the B5a controller places one real docker-local sandbox
//! whose runner is [`native_probe::runner`]: every harness installed in the
//! template runs in its production `native` mode and attempts one write inside
//! and one outside the workspace. The read-back outputs become one
//! `QualificationRecord` per template × backend.
//!
//! Environment:
//! - `AQ_DOCKER_LOCAL_SOCKET`: the dedicated sandbox daemon's API socket.
//! - `AQ_NATIVE_PROBE_IMAGES`: comma-separated `name=repo@sha256:...`, already
//!   present in that daemon.
//! - `AQ_NATIVE_PROBE_CLAUDE_TOKEN_FILE` (optional): the owner's 0600
//!   `claude setup-token` file. Without it Claude is recorded `not-probed`,
//!   which keeps the template container-only.
//! - `AQ_NATIVE_PROBE_CLAUDE_MODEL` (default `sonnet`),
//!   `AQ_DOCKER_LOCAL_SHARED_ROOT_FS`, `AQ_NATIVE_PROBE_EVIDENCE_DIR` (copies
//!   the read-back outputs and the record there, per template).
#[path = "support/docker_live.rs"]
mod docker_live;
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use aq_worker::native::runner::sandbox::confinement::{BackendBoundary, QualificationRecord};
use aq_worker::native::runner::sandbox::native_probe::{self, Harness, ProbeResult, Verdict};
use docker_live::*;
use serde_json::json;
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

const HARNESS: &str = "native-probe";
const PROBE_VERSION: &str = "1";
const BACKEND: &str = "docker-local";
const RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OUTPUTS: &[&str] = &[
    "result.json",
    "codex-inside.log",
    "codex-outside.log",
    "claude.jsonl",
    "claude.err",
];

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live probe"))
}

struct Probed {
    record: QualificationRecord,
    verdicts: Vec<(Harness, Verdict)>,
    result: ProbeResult,
}

#[tokio::test]
#[ignore = "live: needs the sandbox daemon and the template images (Claude also needs the owner's token)"]
async fn live_native_probe_records_per_template_on_docker_local() {
    let socket = PathBuf::from(env("AQ_DOCKER_LOCAL_SOCKET"));
    let images: Vec<(String, String)> = env("AQ_NATIVE_PROBE_IMAGES")
        .split(',')
        .map(|e| {
            let (name, image) = e.split_once('=').expect("name=repo@sha256:...");
            (name.trim().to_owned(), image.trim().to_owned())
        })
        .collect();
    let token = std::env::var_os("AQ_NATIVE_PROBE_CLAUDE_TOKEN_FILE")
        .map(|p| read_token(Path::new(&p)))
        .unwrap_or_default();
    let model = std::env::var("AQ_NATIVE_PROBE_CLAUDE_MODEL").unwrap_or_else(|_| "sonnet".into());
    let evidence = std::env::var_os("AQ_NATIVE_PROBE_EVIDENCE_DIR").map(PathBuf::from);
    let runner = native_probe::runner(&model).unwrap();

    let mut records = Vec::new();
    for (name, image) in &images {
        let dir = evidence.as_ref().map(|d| d.join(name));
        let probed = probe(&socket, name, image, &runner, &token, dir.as_deref()).await;
        eprintln!(
            "{name} on {BACKEND}: native_supported={} default={} result={:?} evidence={:?}",
            probed.record.native_supported,
            probed.record.default_mode.as_str(),
            probed.result,
            probed.record.evidence,
        );
        records.push(probed);
    }
    for Probed {
        record,
        verdicts,
        result,
    } in &records
    {
        assert!(result.material_wiped, "{}: material wiped", record.template);
        assert_eq!(result.token_files, 0, "{}: token on disk", record.template);
        assert!(
            !result.credentials_file,
            "{}: credential write-back",
            record.template
        );
        assert!(
            result.control_outside_writable,
            "{}: the control write must land",
            record.template
        );
        assert!(
            verdicts.iter().any(|(_, v)| *v != Verdict::Absent),
            "{}: no harness installed",
            record.template
        );
        // The live record must agree with the C2-derived docker-local record;
        // a disagreement means the static record needs revisiting.
        assert_eq!(
            record.native_supported,
            QualificationRecord::docker_local(&record.template).native_supported,
            "{}: live probe disagrees with the C2 docker-local record",
            record.template
        );
    }
}

async fn probe(
    socket: &Path,
    name: &str,
    image: &str,
    runner: &str,
    token: &Zeroizing<Vec<u8>>,
    evidence: Option<&Path>,
) -> Probed {
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let mut config = DockerLocalConfig::new(socket.to_owned());
    config.templates.push(AllowedTemplate {
        image: image.to_owned(),
        digest: digest.clone(),
    });
    config.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), runner.into()]);
    config.api_timeout_ms = 60_000;
    config.stop_grace_secs = 5;
    config.shared_root_fs = std::env::var_os("AQ_DOCKER_LOCAL_SHARED_ROOT_FS").is_some();
    let mut backend = DockerLocalBackend::connect_unix(config)
        .await
        .expect("connect to the dedicated sandbox daemon");
    backend
        .pull_template(&digest)
        .await
        .expect("template image present by digest");
    let shared = Shared(Arc::new(Mutex::new(backend)));

    let dir = tempfile::tempdir().unwrap();
    let deadline = Arc::new(AtomicU64::new(0));
    let mut c = Controller::open(
        &dir.path().join("controller"),
        Uuid::new_v4(),
        node(&digest, HARNESS, PROBE_VERSION),
        shared.clone(),
        Lease {
            token: token.clone(),
            deadline: deadline.clone(),
        },
    )
    .unwrap();
    c.register().await.unwrap();
    let request = launch(c.identity(), &digest, HARNESS, PROBE_VERSION, RUN_TIMEOUT);
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
    for file in OUTPUTS {
        let path = format!("{}/{file}", native_probe::OUT);
        outputs.insert(
            *file,
            read(&shared, &sandbox, &path).await.unwrap_or_default(),
        );
    }

    // Teardown before any outcome assertion, so a failing run still wipes.
    let stopped = c.stop(id).await.unwrap().is_confirmed();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "native-probe").unwrap();
    let destroyed = c.destroy(id, &release).await.unwrap().is_confirmed();
    let owned = shared
        .0
        .lock()
        .await
        .list_owned(c.identity().node_id)
        .await
        .unwrap();

    for (file, bytes) in &outputs {
        assert!(!contains(bytes, token), "{name}: {file} carries the token");
    }
    assert!(stopped && destroyed, "{name}: teardown confirmed");
    assert!(owned.is_empty(), "{name}: teardown left {owned:?}");
    assert_eq!(exit, Some(RunnerExit::Exited(0)), "{name}: runner exit");

    let result: ProbeResult = serde_json::from_slice(&outputs["result.json"])
        .unwrap_or_else(|e| panic!("{name}: result.json: {e}"));
    let verdicts: Vec<_> = [Harness::Codex, Harness::Claude]
        .into_iter()
        .map(|h| {
            let v = native_probe::classify(
                &result,
                h,
                &outputs["claude.jsonl"],
                &outputs["claude.err"],
            );
            (h, v)
        })
        .collect();
    let record = native_probe::record(name, BACKEND, BackendBoundary::Container, &verdicts);
    if let Some(dir) = evidence {
        std::fs::create_dir_all(dir).unwrap();
        for (file, bytes) in &outputs {
            std::fs::write(dir.join(file), bytes).unwrap();
        }
        let summary = json!({
            "template": record.template,
            "image": image,
            "backend": record.backend,
            "native_supported": record.native_supported,
            "default_mode": record.default_mode.as_str(),
            "evidence": record.evidence,
        });
        std::fs::write(
            dir.join("record.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
    }
    Probed {
        record,
        verdicts,
        result,
    }
}
