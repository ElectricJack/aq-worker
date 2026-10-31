use aq_worker::{
    clock::FakeClock,
    journal::{Journal, Limits},
    runtime::Runtime,
    source::GitClient,
};
use aq_worker_protocol::{testing::*, *};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;
use uuid::Uuid;
fn limits() -> Limits {
    Limits {
        capacity: Resources {
            slots: 2,
            cpu_millis: 10000,
            memory_bytes: 100000,
            disk_bytes: 8 * 1024 * 1024 * 1024,
        },
        max_pending_results: 16,
        max_pending_bytes: 256 * 1024 * 1024,
        max_preparing: 2,
        minimum_free_disk_bytes: 0,
    }
}
fn offer() -> Offer {
    let mut o = sample_offer();
    o.spec.required_capabilities.push(SOURCE_CAPABILITY.into());
    o.spec.resources.disk_bytes = 1024 * 1024 * 1024;
    o.spec.bindings = Some(ExecutionBindings {
        graph: None,
        source: Some(SourceBinding {
            repository_id: Uuid::from_u128(50),
            binding_generation: 1,
            logical_identity: "synthetic".into(),
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
        }),
    });
    o.spec = o.spec.seal();
    o
}
fn runtime(path: &Path) -> Runtime<FakeClock> {
    Runtime::new(
        Journal::open(path, sample_offer().identity, limits()).unwrap(),
        FakeClock::new(100),
    )
}
struct Peer {
    _temp: TempDir,
    client: GitClient,
    requests: Arc<Mutex<Vec<Value>>>,
}
fn peer(lose_create: bool) -> Peer {
    let temp = TempDir::new().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = temp.path().join("git.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let work = temp
        .path()
        .join("workspaces")
        .join(Uuid::from_u128(70).to_string());
    std::fs::create_dir_all(&work).unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let recorded = requests.clone();
    std::thread::spawn(move || {
        let mut lost = false;
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut line = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            recorded.lock().unwrap().push(req.clone());
            let result = match req["operation"].as_str().unwrap() {
                "capabilities" => {
                    json!({"protocol_version":1,"artifact_format_version":1,"limits":{"artifact":{"max_expanded_bytes":536870912_u64,"max_bundle_bytes":67108864_u64}},"git":{"checkout_profile":"no_attributes_v1"},"operations":["repo.show","artifact.import","workspace.create","workspace.checkpoint","workspace.retire","artifact.export","retention.release","repo.cache-evict"]})
                }
                "repo.show" => {
                    json!({"repository_id":Uuid::from_u128(50),"binding_generation":1,"identity":"synthetic","mode":"worker"})
                }
                "workspace.create" => {
                    if lose_create && !lost {
                        lost = true;
                        continue;
                    }
                    json!({"workspace_id":Uuid::from_u128(70),"generation":1,"path":work})
                }
                "workspace.inspect" => json!({"head":{"algorithm":"sha1","hex":"a".repeat(40)}}),
                "workspace.checkpoint" => {
                    json!({"workspace_id":Uuid::from_u128(70),"workspace_generation":2,"source":{"algorithm":"sha1","hex":"b".repeat(40)},"retention_handle":Uuid::from_u128(80)})
                }
                "workspace.retire" => json!({"retired":true}),
                other => panic!("unexpected operation {other}"),
            };
            let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true,"result":result});
            if let Some(id) = req.get("operation_id") {
                reply["operation_id"] = id.clone();
            }
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            let _ = conn.write_all(&bytes);
        }
    });
    let client = GitClient::new(&socket, temp.path()).unwrap();
    Peer {
        _temp: temp,
        client,
        requests,
    }
}
fn authorize(rt: &mut Runtime<FakeClock>, p: &Peer, o: Offer) {
    let mut coordinator = FakeCoordinator::new(o.clone(), 100);
    let accept = rt.reserve_source(o, &p.client).unwrap();
    rt.authorize(coordinator.accept(&accept).unwrap().grant, 100)
        .unwrap();
}
#[test]
fn source_admission_is_demand_only_and_socket_loss_replays_after_restart() {
    let p = peer(true);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    assert!(rt.reserve(o.clone()).is_err());
    authorize(&mut rt, &p, o);
    assert!(
        !p.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["operation"] == "workspace.create")
    );
    assert!(rt.prepare_source(id, &p.client).is_err());
    assert_eq!(rt.journal.state().attempts[&id].phase, Phase::Preparing);
    assert_eq!(rt.journal.protected_usage().unwrap().slots, 1);
    drop(rt);
    let mut rt = runtime(temp.path());
    let first = rt.prepare_source(id, &p.client).unwrap();
    assert_eq!(first.workspace_id, Uuid::from_u128(70));
    let requests = p.requests.lock().unwrap();
    let creates: Vec<_> = requests
        .iter()
        .filter(|r| r["operation"] == "workspace.create")
        .collect();
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0], creates[1]);
    assert!(rt.retire(id).is_err());
}
#[test]
fn guardian_stopped_writers_gate_checkpoint_and_replay() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    authorize(&mut rt, &p, o);
    rt.prepare_source(id, &p.client).unwrap();
    assert!(rt.checkpoint_source(id, &p.client).is_err());
    let handle = rt
        .launch_fixture(id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(id, handle).unwrap();
    let cp = rt.checkpoint_source(id, &p.client).unwrap();
    assert_eq!(cp.source.hex, "b".repeat(40));
    assert_eq!(
        rt.checkpoint_source(id, &p.client)
            .unwrap()
            .retention_handle,
        cp.retention_handle
    );
    assert!(rt.retire_source(id, &p.client).is_err());
    assert!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .receipt
            .is_none()
    );
    assert_eq!(rt.journal.available().unwrap().slots, 2);
    assert!(rt.journal.protected_usage().unwrap().disk_bytes > 0);
}
#[test]
fn binding_conflicts_and_pending_disk_pressure_refuse_new_work() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let mut o = offer();
    o.spec
        .bindings
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .binding_generation = 2;
    o.spec = o.spec.seal();
    assert!(rt.reserve_source(o, &p.client).is_err());
    let mut o = offer();
    o.spec.resources.disk_bytes = 100001;
    o.spec = o.spec.seal();
    assert!(rt.reserve_source(o, &p.client).is_err());
    assert!(rt.journal.state().attempts.is_empty());
    let o = offer();
    let a = rt.reserve_source(o.clone(), &p.client).unwrap();
    assert_eq!(rt.reserve_source(o.clone(), &p.client).unwrap(), a);
    let mut changed = o;
    changed
        .spec
        .bindings
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .base
        .hex = "c".repeat(40);
    changed.spec = changed.spec.seal();
    assert!(rt.reserve_source(changed, &p.client).is_err());
}

fn direct(
    exe: &Path,
    root: &Path,
    repo: Uuid,
    operation: &str,
    params: Value,
    generation: u64,
) -> Value {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .arg("--state-dir")
        .arg(root)
        .args(["request", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let id = Uuid::new_v4();
    let request = json!({"protocol_version":1,"request_id":id.to_string(),"operation":operation,"operation_id":id,"repository_id":repo,"expected_binding_generation":generation,"params":params});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{}", response);
    response["result"].clone()
}
#[test]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; no sibling checkout"]
fn pinned_git_real_workspace_checkpoint_export_and_feature_refusal() {
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
    let manifest: aq_worker::source::GitManifest =
        serde_json::from_value(out["manifest"].clone()).unwrap();
    assert_eq!(manifest.source, checkpoint.source);
    assert!(rt.journal.state().attempts[&id].pending_bytes > 0);
    assert!(rt.retire_source(id, &git).is_err());
    let manifest_file = std::fs::read(
        worker
            .join("artifacts")
            .join(manifest.transfer_id.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    let transfer = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint: manifest.source.clone(),
        attempt: rt.journal.state().attempts[&id].offer.attempt.clone(),
        resolved_spec_digest: rt.journal.state().attempts[&id]
            .offer
            .spec
            .resolved_spec_digest
            .clone(),
        source: rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
        artifact_id: manifest.transfer_id,
        manifest_digest: format!("{:x}", Sha256::digest(&manifest_file)),
        bundle_digest: manifest.bundle_sha256.clone(),
        manifest_bytes: manifest_file.len() as u64,
        bundle_bytes: manifest.bundle_size,
    };
    rt.bind_source_transfer(id, transfer.clone()).unwrap();
    let receipt = CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: transfer.transfer_id,
        repository_id: rid,
        binding_generation: 1,
        source: checkpoint.source.clone(),
        retention_handle: manifest.transfer_id,
        manifest_digest: manifest.canonical_digest().unwrap(),
    };
    let mut wrong = receipt.clone();
    wrong.transfer_id = Uuid::new_v4();
    assert!(rt.acknowledge_source(id, wrong).is_err());
    let mut wrong = receipt.clone();
    wrong.manifest_digest = "0".repeat(64);
    assert!(rt.acknowledge_source(id, wrong).is_err());
    rt.acknowledge_source(id, receipt).unwrap();
    let report = rt.journal.state().attempts[&id].report.clone().unwrap();
    rt.acknowledge_result(ResultResponse {
        report_id: report.report_id,
        disposition: ResultDisposition::Accepted,
        retention_release: true,
    })
    .unwrap();
    rt.retire_source(id, &git).unwrap();
    rt.retire(id).unwrap();
    assert!(!work.path.exists());
    rt.reclaim_source(id, 1, &git).unwrap();
    assert_eq!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .operations["evict"]
            .result
            .as_ref()
            .unwrap()["lifecycle"],
        "absent"
    );
    // A new independent attempt is permitted only through explicit frozen inputs.
    let mut next = offer();
    next.spec.project_id = Uuid::new_v4();
    next.offer_id = Uuid::new_v4();
    next.attempt.attempt_id = Uuid::new_v4();
    next.attempt.session_id = Uuid::new_v4();
    next.spec.bindings.as_mut().unwrap().source = Some(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
    );
    next.spec = next.spec.seal();
    let next_id = next.attempt.attempt_id;
    let mut peer = FakeCoordinator::new(next.clone(), 100);
    let a = rt.reserve_source(next, &git).unwrap();
    rt.authorize(peer.accept(&a).unwrap().grant, 100).unwrap();
    let old_incoming = worker.join("artifacts/incoming").join(artifact);
    std::fs::create_dir_all(&old_incoming).unwrap();
    for file in ["manifest.json", "objects.bundle"] {
        std::fs::copy(
            central.join("artifacts").join(artifact).join(file),
            old_incoming.join(file),
        )
        .unwrap();
    }
    assert!(
        rt.import_base(next_id, Uuid::parse_str(artifact).unwrap(), &git)
            .is_err()
    );
    assert_eq!(
        rt.journal.state().attempts[&next_id]
            .source
            .as_ref()
            .unwrap()
            .operations[&format!("import-base-{artifact}")]
            .result
            .as_ref()
            .unwrap()["retention_state"],
        "released"
    );
    let fresh = direct(
        exe,
        &central,
        rid,
        "artifact.export",
        json!({"base":manifest.base,"source":c["source"],"retention_handle":c["retention_handle"]}),
        1,
    );
    let fresh_handle = fresh["artifact_handle"].as_str().unwrap();
    let incoming = worker.join("artifacts/incoming").join(fresh_handle);
    std::fs::create_dir_all(&incoming).unwrap();
    for file in ["manifest.json", "objects.bundle"] {
        std::fs::copy(
            central.join("artifacts").join(fresh_handle).join(file),
            incoming.join(file),
        )
        .unwrap();
    }
    rt.import_base(next_id, Uuid::parse_str(fresh_handle).unwrap(), &git)
        .unwrap();
    let second = rt.prepare_source(next_id, &git).unwrap();
    assert_ne!(second.path, work.path);
    assert!(!second.path.join("result.txt").exists());
    std::fs::write(second.path.join(".gitattributes"), "* text\n").unwrap();
    let h = rt
        .launch_fixture(next_id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(next_id, h).unwrap();
    assert!(rt.checkpoint_source(next_id, &git).is_err());
    assert!(rt.reclaim_source(id, 1, &git).is_err());
    drop(guard);
    binary_source_fixture(
        exe,
        &pin,
        temp.path(),
        &worker,
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
    );
}

fn binary_source_fixture(
    exe: &Path,
    pin: &str,
    root: &Path,
    git_root: &Path,
    binding: SourceBinding,
) {
    use axum::{Json, Router, extract::State, routing::post};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut seed = offer();
        seed.attempt.attempt_id = Uuid::new_v4();
        seed.attempt.session_id = Uuid::new_v4();
        seed.spec.bindings.as_mut().unwrap().source = Some(binding);
        seed.spec = seed.spec.seal();
        let shared = Arc::new(Mutex::new(FakeCoordinator::new(seed, 100)));
        async fn register(
            State(p): State<Arc<Mutex<FakeCoordinator>>>,
            Json(r): Json<RegisterRequest>,
        ) -> Json<RegisterResponse> {
            assert!(r.capabilities.contains(&SOURCE_CAPABILITY.into()));
            assert!(!r.capabilities.contains(&SOURCE_TRANSFER_CAPABILITY.into()));
            {
                let mut p = p.lock().unwrap();
                p.offer.identity = r.identity.clone();
                p.active_identity = r.identity.clone();
            }
            Json(RegisterResponse {
                identity: r.identity,
                protocol_version: 2,
                capabilities: r.capabilities,
            })
        }
        async fn poll(
            State(p): State<Arc<Mutex<FakeCoordinator>>>,
            Json(_): Json<PollRequest>,
        ) -> Json<PollResponse> {
            Json(PollResponse {
                server_time_ms: 100,
                offers: vec![p.lock().unwrap().offer.clone()],
                directives: vec![],
            })
        }
        async fn accept(
            State(p): State<Arc<Mutex<FakeCoordinator>>>,
            Json(r): Json<AcceptRequest>,
        ) -> Json<AcceptResponse> {
            Json(p.lock().unwrap().accept(&r).unwrap())
        }
        async fn observe(
            State(p): State<Arc<Mutex<FakeCoordinator>>>,
            Json(r): Json<Observation>,
        ) -> Json<Ack> {
            Json(p.lock().unwrap().observe(&r).unwrap())
        }
        let app = Router::new()
            .route("/v2/worker/register", post(register))
            .route("/v2/worker/poll", post(poll))
            .route("/v2/worker/accept", post(accept))
            .route("/v2/worker/observe", post(observe))
            .with_state(shared);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = root.join("cli.toml");
        aq_worker::config::WorkerConfig {
            server_url: url,
            node_id: Uuid::from_u128(1),
            credential: "synthetic-node-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        }
        .save(&config)
        .unwrap();
        let state = root.join("cli-journal");
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("--config")
            .arg(&config)
            .arg("run-execution")
            .arg("--state-dir")
            .arg(&state)
            .arg("--fixture-agent")
            .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
            .arg("--aq-git-binary")
            .arg(exe)
            .arg("--aq-git-sha256")
            .arg(pin)
            .arg("--aq-git-state-dir")
            .arg(git_root)
            .args(["--exit-after-result", "--idle-timeout-secs", "5"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("source_pending"));
        assert!(!text.contains("result_acknowledged"));
        assert!(!text.contains("synthetic-node-secret"));
        let journal: aq_worker::journal::State =
            serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
        let r = journal.attempts.values().next().unwrap();
        assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
        assert_eq!(r.phase, Phase::Reporting);
        let source = r.source.as_ref().unwrap();
        assert!(
            source.checkpoint.is_some() && source.exported.is_some() && source.receipt.is_none()
        );
        assert!(source.workspace.as_ref().unwrap().path.exists());
        assert!(!r.report.as_ref().unwrap().claimed_success);
        assert_eq!(r.report.as_ref().unwrap().artifacts.len(), 1);
        assert!(r.result.is_none());
        server.abort();
    });
}
#[test]
fn shared_cache_is_charged_once_for_multiple_projects_and_growth_stays_protected() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let first = offer();
    let first_id = first.attempt.attempt_id;
    authorize(&mut rt, &p, first.clone());
    let mut second = offer();
    second.offer_id = Uuid::new_v4();
    second.attempt.attempt_id = Uuid::new_v4();
    second.attempt.session_id = Uuid::new_v4();
    second.spec.project_id = Uuid::new_v4();
    second.spec = second.spec.seal();
    rt.reserve_source(second, &p.client).unwrap();
    assert_eq!(
        rt.journal.protected_usage().unwrap().disk_bytes,
        2 * first.spec.resources.disk_bytes + 512 * 1024 * 1024
    );
    let workspace = rt.prepare_source(first_id, &p.client).unwrap();
    let h = rt
        .launch_fixture(first_id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(first_id, h).unwrap();
    let file = std::fs::File::create(workspace.path.join("oversize-sparse-file")).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    assert!(rt.checkpoint_source(first_id, &p.client).is_err());
    assert!(
        rt.journal.state().attempts[&first_id]
            .source
            .as_ref()
            .unwrap()
            .workspace_bytes
            >= 2 * 1024 * 1024 * 1024
    );
    assert!(rt.journal.protected_usage().unwrap().disk_bytes > 3 * 1024 * 1024 * 1024);
    assert!(
        p.requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r["operation"] != "workspace.checkpoint")
    );
}
