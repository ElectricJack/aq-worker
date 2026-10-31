//! Explicit CLI integration: cold base, concurrent control and retained handoff.
use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use std::{
    collections::BTreeMap,
    process::Command,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
struct Fixture {
    exe: std::path::PathBuf,
    pin: String,
    central: std::path::PathBuf,
    rid: Uuid,
    binding: SourceBinding,
    exported: Value,
}
struct Peer {
    fixture: Arc<Fixture>,
    coordinator: Mutex<FakeCoordinator>,
    grant: Mutex<Option<Grant>>,
    upload: Mutex<Option<(TransferManifest, TransferStatus)>>,
    operations: Mutex<BTreeMap<Uuid, (Value, Value)>>,
    held: AtomicBool,
    release: Notify,
    control_polls: AtomicUsize,
    directive_acked: AtomicBool,
    cancel: bool,
    report: Mutex<Option<ResultReport>>,
    report_lost: AtomicBool,
}
fn json_response(v: impl serde::Serialize) -> Response {
    axum::Json(v).into_response()
}
async fn endpoint(State(s): State<Arc<Peer>>, r: Request<Body>) -> Response {
    assert_eq!(r.headers()["authorization"], "Bearer fixture-cli-token");
    let path = r.uri().path().to_owned();
    let query = r.uri().query().unwrap_or("").to_owned();
    let body = to_bytes(r.into_body(), MAX_CHUNK_BYTES + 65536)
        .await
        .unwrap();
    if path.starts_with("/v1/execution/sources/base/") {
        let offer = s.coordinator.lock().unwrap().offer.clone();
        let native: aq_worker::source::GitManifest =
            serde_json::from_value(s.fixture.exported["manifest"].clone()).unwrap();
        let dir = s
            .fixture
            .central
            .join("artifacts")
            .join(native.transfer_id.to_string());
        if path.ends_with("/manifest") || path.ends_with("/bundle") {
            if path.ends_with("/bundle") && !s.held.swap(true, Ordering::SeqCst) {
                s.release.notified().await;
            }
            let filename = if path.ends_with("/manifest") {
                "manifest.json"
            } else {
                "objects.bundle"
            };
            let bytes = std::fs::read(dir.join(filename)).unwrap();
            let url = reqwest::Url::parse(&format!("http://fixture/?{query}")).unwrap();
            let fields: BTreeMap<_, _> = url.query_pairs().collect();
            let offset: usize = fields["offset"].parse().unwrap();
            let length: usize = fields["limit"].parse().unwrap();
            return bytes[offset..offset + length].to_vec().into_response();
        }
        use sha2::{Digest, Sha256};
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        return json_response(TransferManifest {
            transfer_id: Uuid::new_v4(),
            role: SourceRole::Base,
            checkpoint: s.fixture.binding.base.clone(),
            attempt: offer.attempt,
            resolved_spec_digest: offer.spec.resolved_spec_digest,
            source: s.fixture.binding.clone(),
            artifact_id: native.transfer_id,
            manifest_digest: format!("{:x}", Sha256::digest(&manifest)),
            bundle_digest: native.bundle_sha256,
            manifest_bytes: manifest.len() as u64,
            bundle_bytes: native.bundle_size,
        });
    }
    let value: Value = if body.is_empty() || (path.contains("/sources/") && !query.is_empty()) {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    match path.as_str() {
        "/v2/worker/register" => {
            let r: RegisterRequest = serde_json::from_value(value).unwrap();
            assert!(r.capabilities.contains(&SOURCE_TRANSFER_CAPABILITY.into()));
            let mut p = s.coordinator.lock().unwrap();
            p.offer.identity = r.identity.clone();
            p.active_identity = r.identity.clone();
            json_response(RegisterResponse {
                identity: r.identity,
                protocol_version: 2,
                capabilities: r.capabilities,
            })
        }
        "/v2/worker/poll" => {
            let r: PollRequest = serde_json::from_value(value).unwrap();
            let p = s.coordinator.lock().unwrap();
            let mut directives = vec![];
            let offers = if r.available.slots > 0 {
                vec![p.offer.clone()]
            } else {
                if r.available.cpu_millis == 0 {
                    s.control_polls.fetch_add(1, Ordering::SeqCst);
                }
                if s.held.load(Ordering::SeqCst) {
                    if s.cancel {
                        directives.push(Directive {
                            directive_id: Uuid::from_u128(500),
                            attempt: p.offer.attempt.clone(),
                            kind: DirectiveKind::Cancel,
                        });
                    } else {
                        s.release.notify_one();
                    }
                }
                vec![]
            };
            json_response(PollResponse {
                server_time_ms: 100,
                offers,
                directives,
            })
        }
        "/v2/worker/accept" => {
            let r = serde_json::from_value(value).unwrap();
            let response = s.coordinator.lock().unwrap().accept(&r).unwrap();
            *s.grant.lock().unwrap() = Some(response.grant.clone());
            json_response(response)
        }
        "/v2/worker/renew" => {
            let r: RenewRequest = serde_json::from_value(value).unwrap();
            let mut grant = s.grant.lock().unwrap();
            let g = grant.as_mut().unwrap();
            assert_eq!(r.attempt, g.attempt);
            g.grant_sequence += 1;
            json_response(RenewResponse { grant: g.clone() })
        }
        "/v2/worker/observe" => {
            let r = serde_json::from_value(value).unwrap();
            json_response(s.coordinator.lock().unwrap().observe(&r).unwrap())
        }
        "/v2/worker/directive-ack" => {
            let r: DirectiveAck = serde_json::from_value(value).unwrap();
            assert_eq!(r.directive_id, Uuid::from_u128(500));
            s.directive_acked.store(true, Ordering::SeqCst);
            s.release.notify_one();
            json_response(Ack {
                operation_id: r.operation_id,
            })
        }
        "/v2/worker/reconcile" => {
            let r: ReconcileRequest = serde_json::from_value(value).unwrap();
            r.validate().unwrap();
            assert!(s.cancel);
            assert_eq!(r.attempts.len(), 1);
            assert_eq!(
                r.attempts[0].attempt,
                s.coordinator.lock().unwrap().offer.attempt
            );
            json_response(ReconcileResponse {
                attempts: r
                    .attempts
                    .into_iter()
                    .map(|a| ReconcileDisposition {
                        attempt: a.attempt,
                        action: ReconcileAction::AwaitReport,
                        grant: None,
                        result: None,
                    })
                    .collect(),
                next_cursor: None,
            })
        }
        "/v2/worker/result" => {
            let r: ResultReport = serde_json::from_value(value.clone()).unwrap();
            r.validate().unwrap();
            let mut operations = s.operations.lock().unwrap();
            if let Some((old, response)) = operations.get(&r.report_id) {
                assert_eq!(old, &value);
                return json_response(response);
            }
            assert_eq!(
                r.supersedes_report_id,
                s.report.lock().unwrap().as_ref().map(|r| r.report_id)
            );
            if s.cancel {
                assert_eq!(r.stop_certainty, StopCertainty::NotStarted);
                assert_eq!(r.outcome, ProcessOutcome::Cancelled);
                assert!(r.artifacts.is_empty() && r.pending_operation_ids.is_empty());
                let response = json!(ResultResponse {
                    report_id: r.report_id,
                    disposition: ResultDisposition::RetainedCancelled,
                    retention_release: true
                });
                operations.insert(r.report_id, (value, response));
                *s.report.lock().unwrap() = Some(r);
                s.report_lost.store(true, Ordering::SeqCst);
                return (
                    StatusCode::BAD_GATEWAY,
                    "synthetic intermediary lost committed nonstart response",
                )
                    .into_response();
            }
            let upload = s.upload.lock().unwrap();
            let (m, status) = upload.as_ref().unwrap();
            let receipt = status.receipt.as_ref().unwrap();
            assert_eq!(r.artifacts.len(), 1);
            let a = &r.artifacts[0];
            assert_eq!(a.artifact_id, m.artifact_id);
            assert_eq!(a.digest, m.manifest_digest);
            assert_eq!(a.bytes, m.manifest_bytes + m.bundle_bytes);
            assert_eq!(a.central_receipt, Some(receipt.receipt_id.to_string()));
            assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
            assert!(r.pending_operation_ids.is_empty());
            let response = json!(ResultResponse {
                report_id: r.report_id,
                disposition: ResultDisposition::Accepted,
                retention_release: true
            });
            operations.insert(r.report_id, (value, response));
            *s.report.lock().unwrap() = Some(r);
            s.report_lost.store(true, Ordering::SeqCst);
            (
                StatusCode::BAD_GATEWAY,
                "synthetic intermediary lost committed result response",
            )
                .into_response()
        }
        "/v1/execution/sources/begin" => {
            let m: TransferManifest = serde_json::from_value(value["manifest"].clone()).unwrap();
            m.validate().unwrap();
            let mut upload = s.upload.lock().unwrap();
            if let Some((old, status)) = upload.as_ref() {
                assert_eq!(old, &m);
                return json_response(status);
            }
            let status = TransferStatus {
                transfer_id: m.transfer_id,
                manifest_offset: 0,
                bundle_offset: 0,
                receipt: None,
            };
            std::fs::create_dir_all(
                s.fixture
                    .central
                    .join("artifacts/incoming")
                    .join(m.artifact_id.to_string()),
            )
            .unwrap();
            *upload = Some((m, status.clone()));
            json_response(status)
        }
        "/v1/execution/sources/finalize" => {
            let r: TransferStatusRequest = serde_json::from_value(value).unwrap();
            let mut upload = s.upload.lock().unwrap();
            let (m, status) = upload.as_mut().unwrap();
            assert_eq!(r.transfer_id, m.transfer_id);
            if status.receipt.is_none() {
                assert_eq!(status.manifest_offset, m.manifest_bytes);
                assert_eq!(status.bundle_offset, m.bundle_bytes);
                let native: aq_worker::source::GitManifest = serde_json::from_slice(
                    &std::fs::read(
                        s.fixture
                            .central
                            .join("artifacts/incoming")
                            .join(m.artifact_id.to_string())
                            .join("manifest.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                let result = direct(
                    &s.fixture.exe,
                    &s.fixture.central,
                    s.fixture.rid,
                    "artifact.import",
                    json!({"artifact_handle":m.artifact_id}),
                    1,
                );
                assert_eq!(result["source"], json!(m.checkpoint));
                status.receipt = Some(CentralRetentionReceipt {
                    receipt_id: Uuid::new_v4(),
                    transfer_id: m.transfer_id,
                    repository_id: s.fixture.rid,
                    binding_generation: 1,
                    source: m.checkpoint.clone(),
                    retention_handle: m.artifact_id,
                    manifest_digest: native.canonical_digest().unwrap(),
                });
            }
            json_response(status)
        }
        _ if path.starts_with("/v1/execution/sources/") && !query.is_empty() => {
            let u = reqwest::Url::parse(&format!("http://fixture/?{query}")).unwrap();
            let fields: BTreeMap<_, _> = u.query_pairs().collect();
            let op = Uuid::parse_str(&fields["operation_id"]).unwrap();
            let mut operations = s.operations.lock().unwrap();
            let request = json!({"query":query,"bytes":body.to_vec()});
            if let Some((old, response)) = operations.get(&op) {
                assert_eq!(old, &request);
                return json_response(response);
            }
            let mut upload = s.upload.lock().unwrap();
            let (m, status) = upload.as_mut().unwrap();
            let manifest = path.ends_with("/manifest");
            let offset = if manifest {
                &mut status.manifest_offset
            } else {
                &mut status.bundle_offset
            };
            assert_eq!(*offset, fields["offset"].parse::<u64>().unwrap());
            use sha2::{Digest, Sha256};
            assert_eq!(fields["digest"], format!("{:x}", Sha256::digest(&body)));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(
                    s.fixture
                        .central
                        .join("artifacts/incoming")
                        .join(m.artifact_id.to_string())
                        .join(if manifest {
                            "manifest.json"
                        } else {
                            "objects.bundle"
                        }),
                )
                .unwrap();
            f.write_all(&body).unwrap();
            f.sync_all().unwrap();
            *offset += body.len() as u64;
            let response = json!(status);
            operations.insert(op, (request, response.clone()));
            json_response(response)
        }
        _ => panic!("unexpected CLI request {path}: {value}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; actual CLI HTTP integration"]
async fn pinned_git_cli_cold_download_handoff_and_held_download_cancel() {
    let exe = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set pinned executable");
    let exe = Path::new(&exe);
    let temp = TempDir::new().unwrap();
    let central = temp.path().join("central");
    let worker = temp.path().join("worker");
    let repo = temp.path().join("repository");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base"
            ])
            .status()
            .unwrap()
            .success()
    );
    let base = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let rid = Uuid::new_v4();
    let oid = json!({"algorithm":"sha1","hex":base});
    direct(
        exe,
        &central,
        rid,
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"central","binding_generation":1,"remote_url":repo,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}}),
        0,
    );
    direct(exe, &central, rid, "repo.sync", json!({}), 1);
    let w = direct(
        exe,
        &central,
        rid,
        "workspace.create",
        json!({"base":oid}),
        1,
    );
    let c = direct(
        exe,
        &central,
        rid,
        "workspace.checkpoint",
        json!({"workspace_id":w["workspace_id"],"expected_generation":w["generation"],"expected_head":oid,"quiescent":true}),
        1,
    );
    let exported = direct(
        exe,
        &central,
        rid,
        "artifact.export",
        json!({"base":oid,"source":c["source"],"retention_handle":c["retention_handle"]}),
        1,
    );
    direct(
        exe,
        &worker,
        rid,
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"worker","binding_generation":1}}),
        0,
    );

    use sha2::{Digest, Sha256};
    let pin = format!("{:x}", Sha256::digest(std::fs::read(exe).unwrap()));
    let fixture = Arc::new(Fixture {
        exe: exe.to_owned(),
        pin,
        central,
        rid,
        binding: SourceBinding {
            repository_id: rid,
            binding_generation: 1,
            logical_identity: "fixture".into(),
            base: serde_json::from_value(oid).unwrap(),
        },
        exported,
    });
    run_case(fixture.clone(), temp.path(), &worker, false).await;
    let cancelled_worker = temp.path().join("worker-cancel");
    direct(
        exe,
        &cancelled_worker,
        rid,
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"worker","binding_generation":1}}),
        0,
    );
    run_case(fixture, temp.path(), &cancelled_worker, true).await;
}
async fn run_case(fixture: Arc<Fixture>, root: &Path, worker: &Path, cancel: bool) {
    std::fs::write(worker.join("artifact_limits.json"),serde_json::to_vec(&json!({"max_bundle_bytes":67108864,"max_expanded_bytes":536870912,"disk_reserve_bytes":16777216})).unwrap()).unwrap();
    let mut seed = offer();
    seed.spec.bindings.as_mut().unwrap().source = Some(fixture.binding.clone());
    seed.spec
        .required_capabilities
        .push(SOURCE_TRANSFER_CAPABILITY.into());
    seed.spec = seed.spec.seal();
    let peer = Arc::new(Peer {
        fixture: fixture.clone(),
        coordinator: Mutex::new(FakeCoordinator::new(seed, 100)),
        grant: Mutex::new(None),
        upload: Mutex::new(None),
        operations: Mutex::new(BTreeMap::new()),
        held: AtomicBool::new(false),
        release: Notify::new(),
        control_polls: AtomicUsize::new(0),
        directive_acked: AtomicBool::new(false),
        cancel,
        report: Mutex::new(None),
        report_lost: AtomicBool::new(false),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = aq_worker::config::WorkerConfig {
        server_url: format!("http://{}", listener.local_addr().unwrap()),
        node_id: Uuid::from_u128(1),
        credential: "fixture-cli-token".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let app = Router::new().fallback(endpoint).with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let label = if cancel { "cancel" } else { "complete" };
    let config_path = root.join(format!("cli-{label}.toml"));
    config.save(&config_path).unwrap();
    let state = root.join(format!("cli-{label}-journal"));
    let output = launch_cli(&fixture, &config_path, &state, worker).await;
    let journal: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
    assert_eq!(
        journal.attempts.len(),
        1,
        "no new offers admitted during bulk transfer"
    );
    let r = journal.attempts.values().next().unwrap();
    let source = r.source.as_ref().unwrap();
    assert!(peer.held.load(Ordering::SeqCst));
    assert!(
        peer.control_polls.load(Ordering::SeqCst) > 0,
        "held download must not block control"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-cli-token"));
    if cancel {
        assert!(peer.directive_acked.load(Ordering::SeqCst));
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("source preparation cancelled or expired")
        );
        assert!(!source.download.as_ref().unwrap().verified);
        assert_eq!(
            source.download.as_ref().unwrap().manifest_offset,
            source.download.as_ref().unwrap().manifest.manifest_bytes
        );
        assert_eq!(source.download.as_ref().unwrap().bundle_offset, 0);
        assert!(r.launch_id.is_none() && !r.start_intent);
        assert!(source.workspace.is_none());
        assert!(!source.operations.contains_key("create"));
        assert!(
            source
                .operations
                .keys()
                .all(|k| !k.starts_with("import-base-"))
        );
        assert!(r.result.is_none());
        assert!(peer.report.lock().unwrap().is_none());
        assert!(r.pending_bytes > 0);
        let incoming = worker.join("artifacts/incoming").join(
            source
                .download
                .as_ref()
                .unwrap()
                .manifest
                .artifact_id
                .to_string(),
        );
        assert!(incoming.is_dir());
        let unknown = launch_cli(&fixture, &config_path, &state, worker).await;
        assert!(!unknown.status.success());
        assert!(String::from_utf8_lossy(&unknown.stderr).contains("502"));
        assert!(
            incoming.is_dir(),
            "lost release response must preserve partial input"
        );
        let unknown_state: aq_worker::journal::State =
            serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
        let held_record = unknown_state.attempts.values().next().unwrap();
        assert!(held_record.result.is_none());
        assert!(held_record.pending_bytes > 0);
        assert_eq!(
            held_record.report.as_ref(),
            peer.report.lock().unwrap().as_ref()
        );
        let recovered = launch_cli(&fixture, &config_path, &state, worker).await;
        assert!(
            recovered.status.success(),
            "{} {}",
            String::from_utf8_lossy(&recovered.stdout),
            String::from_utf8_lossy(&recovered.stderr)
        );
        let recovered_state: aq_worker::journal::State =
            serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
        assert_eq!(recovered_state.attempts.len(), 1);
        let final_record = recovered_state.attempts.values().next().unwrap();
        assert_eq!(final_record.phase, Phase::Retired);
        assert_eq!(final_record.stop_certainty, StopCertainty::NotStarted);
        assert_eq!(final_record.pending_bytes, 0);
        assert!(final_record.source.as_ref().unwrap().retired);
        assert!(final_record.result.as_ref().unwrap().retention_release);
        assert_eq!(
            final_record.result.as_ref().unwrap().disposition,
            ResultDisposition::RetainedCancelled
        );
        assert!(!incoming.exists());
        assert!(final_record.launch_id.is_none());
        assert!(
            final_record
                .source
                .as_ref()
                .unwrap()
                .operations
                .keys()
                .all(|k| !k.starts_with("import-base-") && k != "create")
        );
        assert!(peer.report_lost.load(Ordering::SeqCst));
    } else {
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(r.phase, Phase::Retired);
        assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
        assert!(source.download.as_ref().unwrap().verified);
        assert!(source.receipt.is_some());
        assert!(source.report_sealed);
        assert!(r.result.as_ref().unwrap().retention_release);
        assert!(peer.report_lost.load(Ordering::SeqCst));
        assert!(
            peer.report
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supersedes_report_id
                .is_none()
        );
        assert!(!source.workspace.as_ref().unwrap().path.exists());
        let checkpoint = source.checkpoint.as_ref().unwrap();
        direct(
            &fixture.exe,
            &fixture.central,
            fixture.rid,
            "workspace.create",
            json!({"base":checkpoint.source}),
            1,
        );
    }
    server.abort();
}

async fn launch_cli(
    fixture: &Fixture,
    config_path: &Path,
    state: &Path,
    worker: &Path,
) -> std::process::Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("--config")
            .arg(config_path)
            .kill_on_drop(true)
            .arg("run-execution")
            .arg("--state-dir")
            .arg(state)
            .arg("--fixture-agent")
            .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
            .arg("--aq-git-binary")
            .arg(&fixture.exe)
            .arg("--aq-git-sha256")
            .arg(&fixture.pin)
            .arg("--aq-git-state-dir")
            .arg(worker)
            .args([
                "--source-transfer",
                "--exit-after-result",
                "--idle-timeout-secs",
                "10",
            ])
            .output(),
    )
    .await
    .expect("CLI timeout")
    .unwrap()
}
