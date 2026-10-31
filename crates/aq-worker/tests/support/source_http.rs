//! Explicit local public-Git plus HTTP qualification; no live daemon/provider.
use super::*;
use aq_worker::{
    config::WorkerConfig, execution_client::ExecutionClient, source_transfer::UploadAction,
    transfer_client::TransferClient,
};
use std::{
    collections::BTreeMap,
    io::Read,
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; local HTTP qualification"]
async fn pinned_git_http_retention_lost_finalize_and_report_restart() {
    use std::process::Command;
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
    let artifact = exported["artifact_handle"].as_str().unwrap();
    let incoming = worker.join("artifacts/incoming").join(artifact);
    std::fs::create_dir_all(&incoming).unwrap();
    for file in ["manifest.json", "objects.bundle"] {
        std::fs::copy(
            central.join("artifacts").join(artifact).join(file),
            incoming.join(file),
        )
        .unwrap();
    }
    std::fs::write(worker.join("artifact_limits.json"),serde_json::to_vec(&json!({"max_bundle_bytes":64*1024*1024,"max_expanded_bytes":512*1024*1024,"disk_reserve_bytes":16*1024*1024})).unwrap()).unwrap();
    use sha2::{Digest, Sha256};
    let pin = format!("{:x}", Sha256::digest(std::fs::read(exe).unwrap()));
    assert!(aq_worker::source::GitService::start(exe, &"0".repeat(64), &worker).is_err());
    let guard = aq_worker::source::GitService::start(exe, &pin, &worker).unwrap();
    let git = guard.client.clone();
    let journal = temp.path().join("journal");
    let mut rt = runtime(&journal);
    let mut o = offer();
    o.spec.bindings.as_mut().unwrap().source = Some(SourceBinding {
        repository_id: rid,
        binding_generation: 1,
        logical_identity: "fixture".into(),
        base: serde_json::from_value(oid).unwrap(),
    });
    o.spec = o.spec.seal();
    let id = o.attempt.attempt_id;
    let mut coordinator = FakeCoordinator::new(o.clone(), 100);
    let a = rt.reserve_source(o, &git).unwrap();
    rt.authorize(coordinator.accept(&a).unwrap().grant, 100)
        .unwrap();
    rt.import_base(id, Uuid::parse_str(artifact).unwrap(), &git)
        .unwrap();
    let work = rt.prepare_source(id, &git).unwrap();
    std::fs::write(work.path.join("result.txt"), "retained source").unwrap();
    let h = rt
        .launch_fixture(id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(id, h).unwrap();
    let checkpoint = rt.checkpoint_source(id, &git).unwrap();
    assert_ne!(checkpoint.source.hex, base);
    let out = rt.export_source(id, &git).unwrap();
    let work = &work.path;

    let manifest: aq_worker::source::GitManifest =
        serde_json::from_value(out["manifest"].clone()).unwrap();
    rt.prepare_source_upload(id, &git).unwrap();
    let m = rt.journal.state().attempts[&id]
        .source
        .as_ref()
        .unwrap()
        .transfer
        .clone()
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(PeerState {
        manifest: None,
        status: None,
        operations: BTreeMap::new(),
        report: None,
        finalize_lost: false,
        report_lost: false,
    }));
    let shared = state.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let exe = exe.to_owned();
    let exe_check = exe.clone();
    let central = central.to_owned();
    let central_check = central.clone();
    let server = std::thread::spawn(move || {
        for c in listener.incoming() {
            let mut c = c.unwrap();
            if done.load(Ordering::SeqCst) {
                break;
            }
            c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(c.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut size = 0;
            let mut auth = false;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("content-length") {
                    size = value.trim().parse().unwrap();
                }
                if name.eq_ignore_ascii_case("authorization") {
                    auth = value.trim() == "Bearer fixture-http-node";
                }
            }
            assert!(auth);
            assert!(size <= MAX_CHUNK_BYTES + 65536);
            let mut body = vec![0; size];
            reader.read_exact(&mut body).unwrap();
            let path = first.split_whitespace().nth(1).unwrap();
            let mut s = shared.lock().unwrap();
            if path.ends_with("/begin") {
                let request: Value = serde_json::from_slice(&body).unwrap();
                let m: TransferManifest =
                    serde_json::from_value(request["manifest"].clone()).unwrap();
                m.validate().unwrap();
                assert_eq!(m.source.repository_id, rid);
                if let Some(old) = &s.manifest {
                    assert_eq!(old, &m);
                } else {
                    s.status = Some(TransferStatus {
                        transfer_id: m.transfer_id,
                        manifest_offset: 0,
                        bundle_offset: 0,
                        receipt: None,
                    });
                    s.manifest = Some(m.clone());
                    std::fs::create_dir_all(
                        central
                            .join("artifacts/incoming")
                            .join(m.artifact_id.to_string()),
                    )
                    .unwrap();
                }
                respond(&mut c, &json!(s.status), false);
            } else if path.contains('?') {
                let (route, query) = path.split_once('?').unwrap();
                let fields: BTreeMap<_, _> = query
                    .split('&')
                    .map(|v| v.split_once('=').unwrap())
                    .collect();
                let op = Uuid::parse_str(fields["operation_id"]).unwrap();
                if let Some((old, response)) = s.operations.get(&op) {
                    assert_eq!(old, &body);
                    respond(&mut c, response, false);
                    continue;
                }
                let m = s.manifest.as_ref().unwrap().clone();
                let offset: u64 = fields["offset"].parse().unwrap();
                use sha2::{Digest, Sha256};
                assert_eq!(fields["digest"], format!("{:x}", Sha256::digest(&body)));
                let file = route.rsplit('/').next().unwrap();
                let (filename, current) = match file {
                    "manifest" => ("manifest.json", s.status.as_ref().unwrap().manifest_offset),
                    "bundle" => ("objects.bundle", s.status.as_ref().unwrap().bundle_offset),
                    _ => panic!("unexpected chunk route"),
                };
                assert_eq!(offset, current);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(
                        central
                            .join("artifacts/incoming")
                            .join(m.artifact_id.to_string())
                            .join(filename),
                    )
                    .unwrap();
                f.write_all(&body).unwrap();
                f.sync_all().unwrap();
                let status = s.status.as_mut().unwrap();
                if file == "manifest" {
                    status.manifest_offset += body.len() as u64;
                } else {
                    status.bundle_offset += body.len() as u64;
                }
                let response = json!(status);
                s.operations.insert(op, (body, response.clone()));
                respond(&mut c, &response, false);
            } else if path.ends_with("/finalize") {
                let request: TransferStatusRequest = serde_json::from_slice(&body).unwrap();
                request.validate().unwrap();
                if let Some((old, response)) = s.operations.get(&request.operation_id) {
                    assert_eq!(old, &body);
                    respond(&mut c, response, false);
                    continue;
                }
                let m = s.manifest.as_ref().unwrap().clone();
                assert_eq!(request.transfer_id, m.transfer_id);
                assert_eq!(s.status.as_ref().unwrap().manifest_offset, m.manifest_bytes);
                assert_eq!(s.status.as_ref().unwrap().bundle_offset, m.bundle_bytes);
                let native: aq_worker::source::GitManifest = serde_json::from_slice(
                    &std::fs::read(
                        central
                            .join("artifacts/incoming")
                            .join(m.artifact_id.to_string())
                            .join("manifest.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                let imported = direct(
                    &exe,
                    &central,
                    rid,
                    "artifact.import",
                    json!({"artifact_handle":m.artifact_id}),
                    1,
                );
                assert_eq!(imported["source"], json!(m.checkpoint));
                s.status.as_mut().unwrap().receipt = Some(CentralRetentionReceipt {
                    receipt_id: Uuid::new_v4(),
                    transfer_id: m.transfer_id,
                    repository_id: rid,
                    binding_generation: 1,
                    source: m.checkpoint,
                    retention_handle: m.artifact_id,
                    manifest_digest: native.canonical_digest().unwrap(),
                });
                let response = json!(s.status);
                s.operations
                    .insert(request.operation_id, (body, response.clone()));
                s.finalize_lost = true;
                respond(&mut c, &response, true);
            } else if path == "/v2/worker/result" {
                let report: ResultReport = serde_json::from_slice(&body).unwrap();
                report.validate().unwrap();
                if let Some((old, response)) = s.operations.get(&report.report_id) {
                    if old != &body {
                        refuse(&mut c);
                    } else {
                        respond(&mut c, response, false);
                    }
                    continue;
                }
                if report.supersedes_report_id != s.report.as_ref().map(|r| r.report_id) {
                    refuse(&mut c);
                    continue;
                }
                let m = s.manifest.as_ref().unwrap();
                let receipt = s.status.as_ref().unwrap().receipt.as_ref().unwrap();
                assert_eq!(report.attempt, m.attempt);
                assert_eq!(report.resolved_spec_digest, m.resolved_spec_digest);
                assert_eq!(report.stop_certainty, StopCertainty::ConfirmedStopped);
                assert_eq!(report.artifacts.len(), 1);
                let a = &report.artifacts[0];
                assert_eq!(a.artifact_id, m.artifact_id);
                assert_eq!(a.digest, m.manifest_digest);
                assert_eq!(a.bytes, m.manifest_bytes + m.bundle_bytes);
                if a.central_receipt != Some(receipt.receipt_id.to_string()) {
                    refuse(&mut c);
                    continue;
                }
                assert!(report.pending_operation_ids.is_empty());
                let response = json!(ResultResponse {
                    report_id: report.report_id,
                    disposition: ResultDisposition::Accepted,
                    retention_release: true
                });
                s.operations
                    .insert(report.report_id, (body, response.clone()));
                s.report = Some(report);
                s.report_lost = true;
                respond(&mut c, &response, true);
            } else {
                panic!("unexpected HTTP route {path}");
            }
        }
    });
    let config = WorkerConfig {
        server_url: format!("http://{addr}"),
        node_id: rt.journal.state().identity.node_id,
        credential: "fixture-http-node".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let client = TransferClient::from_config(&config, 2).unwrap();
    while let Some(action) = rt.next_source_upload(id).unwrap() {
        let result = action.execute(&client).await;
        if matches!(action, UploadAction::Finalize(_)) && !state.lock().unwrap().finalize_lost {
            panic!("finalize did not reach peer");
        }
        match result {
            Ok(status) => rt.acknowledge_source_upload(id, &action, status).unwrap(),
            Err(_) => {
                assert!(matches!(action, UploadAction::Finalize(_)));
                assert!(rt.retire_source(id, &git).is_err());
                drop(rt);
                rt = runtime(&journal);
                let retry = rt.next_source_upload(id).unwrap().unwrap();
                match (&action, &retry) {
                    (UploadAction::Finalize(a), UploadAction::Finalize(b)) => assert_eq!(a, b),
                    _ => panic!("restart changed operation"),
                };
                let status = retry.execute(&client).await.unwrap();
                rt.acknowledge_source_upload(id, &retry, status).unwrap();
            }
        }
    }
    assert!(
        rt.retire_source(id, &git).is_err(),
        "retention is not deletion permission"
    );
    let execution = ExecutionClient::from_config(&config).unwrap();
    let local_only = rt.journal.state().attempts[&id].report.clone().unwrap();
    assert!(local_only.supersedes_report_id.is_some());
    assert!(execution.report(&local_only).await.is_err());
    assert!(state.lock().unwrap().report.is_none());
    let report = rt.prepare_source_report(id).unwrap();
    assert!(report.supersedes_report_id.is_none());
    let mut unretained = report.clone();
    unretained.report_id = Uuid::new_v4();
    unretained.artifacts[0].central_receipt = None;
    assert!(execution.report(&unretained).await.is_err());
    assert!(state.lock().unwrap().report.is_none());
    assert!(execution.report(&report).await.is_err());
    assert!(state.lock().unwrap().report_lost);
    let mut conflicting = report.clone();
    conflicting.output.push_str("changed after commit");
    assert!(execution.report(&conflicting).await.is_err());
    assert!(rt.retire_source(id, &git).is_err());
    drop(rt);
    rt = runtime(&journal);
    let exact = rt.prepare_source_report(id).unwrap();
    assert_eq!(exact, report);
    let response = execution.report(&exact).await.unwrap();
    rt.acknowledge_result(response).unwrap();
    rt.retire_source(id, &git).unwrap();
    rt.retire(id).unwrap();
    assert!(!work.exists());
    rt.reclaim_source(id, 1, &git).unwrap();
    assert!(
        !worker
            .join("artifacts")
            .join(m.artifact_id.to_string())
            .exists()
    );
    let retained = direct(
        &exe_check,
        &central_check,
        rid,
        "workspace.create",
        json!({"base":manifest.source}),
        1,
    );
    assert!(
        Path::new(retained["path"].as_str().unwrap())
            .join("result.txt")
            .exists()
    );
    stop.store(true, Ordering::SeqCst);
    drop(TcpStream::connect(addr).unwrap());
    server.join().unwrap();
}

struct PeerState {
    manifest: Option<TransferManifest>,
    status: Option<TransferStatus>,
    operations: BTreeMap<Uuid, (Vec<u8>, Value)>,
    report: Option<ResultReport>,
    finalize_lost: bool,
    report_lost: bool,
}
fn respond(stream: &mut TcpStream, body: &Value, lost: bool) {
    let bytes = serde_json::to_vec(body).unwrap();
    let length = if lost { bytes.len() + 1 } else { bytes.len() };
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n").unwrap();
    stream.write_all(&bytes).unwrap();
}

fn refuse(stream: &mut TcpStream) {
    let body = b"{\"error\":\"synthetic contract refusal\"}";
    write!(stream,"HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",body.len()).unwrap();
    stream.write_all(body).unwrap();
}
