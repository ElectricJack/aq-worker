//! Synthetic authenticated coordinator and finite public aq-git socket peer.
//! Real Git bundle qualification is separately explicit/ignored by default.
use super::*;
use crate::{
    native::{
        http::{NativeClient, control::Preparation},
        materials::MaterialStore,
        preparation::InputPipeline,
    },
    source::{GitClient, GitManifest},
};
use aq_admission::{
    ArtifactKind,
    control::{ControlReply, PreparationDelivery},
    execution_spec::ProfiledExecutionSpec,
    transport::{ArtifactFetch, DeliveryClock, MaterialDelivery, MaterialFetch},
};
use aq_worker_protocol::{
    AttemptRef, ObjectAlgorithm, ObjectId, SourceBinding, SourceRole, TransferManifest,
};
use axum::{
    Json, Router,
    extract::{Path as HttpPath, Query},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};
struct Export {
    manifest: TransferManifest,
    manifest_bytes: Vec<u8>,
    bundle: Vec<u8>,
}
fn synthetic() -> Export {
    let id = Uuid::new_v4();
    let source = SourceBinding {
        repository_id: Uuid::from_u128(41),
        binding_generation: 1,
        logical_identity: "synthetic".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    };
    let bundle = vec![b'x'; 300000];
    let bd = format!("{:x}", Sha256::digest(&bundle));
    let native = GitManifest {
        format_version: 1,
        repository_id: source.repository_id,
        repository_identity: source.logical_identity.clone(),
        binding_generation: 1,
        transfer_id: id,
        object_format: "sha1".into(),
        base: source.base.clone(),
        source: source.base.clone(),
        prerequisites: vec![],
        bundle_size: bundle.len() as u64,
        bundle_sha256: Some(bd.clone()),
        producer_installation_id: Uuid::new_v4(),
        reference_only: false,
        caller_correlation: json!({}),
    };
    let bytes = serde_json::to_vec(&native).unwrap();
    let manifest = TransferManifest {
        transfer_id: id,
        artifact_id: id,
        role: SourceRole::Base,
        attempt: AttemptRef {
            attempt_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(1),
            generation: 1,
        },
        source: source.clone(),
        checkpoint: source.base,
        resolved_spec_digest: "a".repeat(64),
        manifest_bytes: bytes.len() as u64,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        bundle_bytes: bundle.len() as u64,
        bundle_digest: Some(bd),
    };
    Export {
        manifest,
        manifest_bytes: bytes,
        bundle,
    }
}
struct Inputs {
    pipeline: Option<Arc<InputPipeline>>,
    preparation: Arc<Preparation>,
    client: NativeClient,
    reference: ArtifactRef,
    id: Uuid,
    root: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
    mode: Arc<AtomicU8>,
    range_entered: Arc<AtomicBool>,
    inline_entered: Arc<tokio::sync::Semaphore>,
    inline_release: Arc<tokio::sync::Semaphore>,
    inline_max_active: Arc<AtomicUsize>,
    grant_mode: Arc<AtomicU8>,
    grant_window_ms: Arc<AtomicU64>,
}
struct ActiveInline(Arc<AtomicUsize>);
impl Drop for ActiveInline {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Drop for Inputs {
    fn drop(&mut self) {
        self.task.abort()
    }
}
impl Inputs {
    fn pipeline(&self) -> &Arc<InputPipeline> {
        self.pipeline.as_ref().unwrap()
    }
    async fn new(mut export: Export) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        let materials = dir.path().join("materials");
        fs::create_dir(&materials).unwrap();
        fs::set_permissions(&materials, fs::Permissions::from_mode(0o700)).unwrap();
        let mut offered = offer();
        offered.offered.reservation_ttl_ms = 30000;
        let mut spec: ProfiledExecutionSpec = serde_json::from_value(
            serde_json::from_slice::<Value>(include_bytes!(
                "../../../../../vendor/aq-admission-0.1.7/artifacts/execution-spec.fixture.json"
            ))
            .unwrap()["spec"]
                .clone(),
        )
        .unwrap();
        spec.source.repository_id = export.manifest.source.repository_id;
        spec.source.resolved_object.hex = export.manifest.source.base.hex.clone();
        let bytes = spec.canonical_bytes().unwrap();
        let reference = spec.artifact().unwrap();
        offered.offered.offer.binding.execution_spec_digest = reference.digest.clone();
        let mut descriptor = offered.offered.artifacts[0].clone();
        descriptor.kind = ArtifactKind::Composition;
        descriptor.artifact = reference.clone();
        descriptor.bytes = bytes.len() as u64;
        offered.offered.artifacts.push(descriptor.clone());
        let mut observation = offered
            .offered
            .artifacts
            .iter()
            .find(|d| d.artifact == spec.task_context)
            .unwrap()
            .clone();
        observation.artifact = spec.task_observation.clone();
        observation.bytes = observation.artifact.id.as_str().len() as u64;
        offered.offered.artifacts.push(observation);
        let inline: Vec<_> = offered
            .offered
            .artifacts
            .iter()
            .map(|d| {
                let body = if d.artifact == reference {
                    bytes.clone()
                } else if d.artifact == offered.startup.artifact().unwrap() {
                    canonical_bytes(&offered.startup).unwrap()
                } else if d.artifact == offered.startup.payload {
                    b"instructions\n\ntask-context".to_vec()
                } else {
                    d.artifact.id.as_str().as_bytes().to_vec()
                };
                assert_eq!(Digest::of(&body), d.artifact.digest);
                assert_eq!(body.len() as u64, d.bytes);
                (d.clone(), body)
            })
            .collect();
        offered.validate().unwrap();
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut journal = Journal::create_identity(&root, identity(), capacity).unwrap();
        let id = journal
            .reserve(
                offered.clone(),
                qualification(),
                boot_ns().unwrap() + 29_000_000_000,
                32 * 1024 * 1024 * 1024,
            )
            .unwrap();
        let accept = journal.begin_accept(id, capabilities()).unwrap();
        let cap = journal.state().attempts[&id].reservation.deadline_ns;
        export.manifest.resolved_spec_digest = reference.digest.as_str().into();
        let mut delivery: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        delivery.original.clock = DeliveryClock::new(1500, 29000).unwrap();
        delivery.delivery.clock = DeliveryClock::new(1600, 29000).unwrap();
        delivery.original.binding = offered.offered.offer.binding.clone();
        delivery.original.materials[0].binding = offered.offered.offer.binding.clone();
        delivery.original.materials[0].expires_at_ms = 30000;
        let material_lease = delivery.original.materials[0].clone();
        let mode = Arc::new(AtomicU8::new(0));
        let r1 = root.clone();
        let r2 = root.clone();
        let m = export.manifest.clone();
        let status = m.clone();
        let range_mode = mode.clone();
        let range_entered = Arc::new(AtomicBool::new(false));
        let entered = range_entered.clone();
        let grant_plan = delivery.original.clone();
        let grant_mode = Arc::new(AtomicU8::new(0));
        let grant_window_ms = Arc::new(AtomicU64::new(1_000));
        let grant_case = grant_mode.clone();
        let grant_window = grant_window_ms.clone();
        let grant_id = Uuid::new_v4();
        let prepare_route =
            move |headers: HeaderMap, Json(request): Json<aq_admission::transport::Prepare>| {
                let plan = grant_plan.clone();
                let mode = grant_case.clone();
                let window = grant_window.clone();
                async move {
                    let mut grant: aq_admission::control::InitialGrantDelivery =
                        serde_json::from_value(fixture()["replies"][2]["body"].clone()).unwrap();
                    grant.operation_id = request.operation_id;
                    grant.original.grant.binding = request.binding.clone();
                    grant.original.grant.grant_id = if mode.load(Ordering::SeqCst) == 1 {
                        Uuid::new_v4()
                    } else {
                        grant_id
                    };
                    grant.original.guardian_id = request.guardian_id;
                    grant.original.launch_id = request.launch_id;
                    grant.original.reservation_id = request.reservation.reservation_id;
                    grant.original.materials = plan.materials;
                    grant.original.clock =
                        DeliveryClock::new(2_000, 2_000 + window.load(Ordering::SeqCst)).unwrap();
                    grant.original.grant.expires_at_ms = grant.original.clock.expires_at_ms;
                    grant.execution_deadline_ms = 22_000;
                    grant.delivery.clock = grant.original.clock.clone();
                    grant.delivery.nonce =
                        Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                    Json(ControlReply::InitialGrant(grant))
                }
            };
        let accept_route = move |headers: HeaderMap, Json(request): Json<Accept>| {
            let mut d = delivery.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                d.delivery.nonce =
                    Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                d.original.operation_id = request.operation_id;
                d.original.reservation = request.reservation;
                d.original.guardian_id = request.guardian_id;
                d.original.launch_id = request.launch_id;
                Json(ControlReply::Preparation(d))
            }
        };
        let artifact_mode = mode.clone();
        let inline_entered = Arc::new(tokio::sync::Semaphore::new(0));
        let inline_release = Arc::new(tokio::sync::Semaphore::new(0));
        let inline_max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let inline_signals = (
            inline_entered.clone(),
            inline_release.clone(),
            inline_max_active.clone(),
        );
        let artifact_route = move |Json(request): Json<ArtifactFetch>| {
            let (descriptor, bytes) = inline
                .iter()
                .find(|(d, _)| d.artifact == request.artifact)
                .unwrap()
                .clone();
            let root = r1.clone();
            let artifact_mode = artifact_mode.clone();
            let active = active.clone();
            let (entered, release, maximum) = inline_signals.clone();
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _active = ActiveInline(active);
                maximum.fetch_max(current, Ordering::SeqCst);
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap();
                assert!(
                    snapshot.state.attempts[&request.binding.attempt_id]
                        .downloads
                        .values()
                        .any(|d| d.requests.iter().any(|r| same(r, &request).unwrap()))
                );
                if artifact_mode.load(Ordering::SeqCst) == 4 {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                if artifact_mode.load(Ordering::SeqCst) == 3
                    && request.artifact.id.as_str() == "instructions"
                {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "synthetic partial inline batch",
                    )
                        .into_response();
                }
                let end =
                    (request.offset + u64::from(request.length)).min(bytes.len() as u64) as usize;
                let mut response = bytes[request.offset as usize..end].to_vec().into_response();
                artifact_headers(&mut response, &descriptor, request.offset);
                response
            }
        };
        let status_route = move |HttpPath(id): HttpPath<Uuid>, headers: HeaderMap| {
            let m = status.clone();
            let root = r2.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap();
                assert!(snapshot.state.attempts[&id].source.is_some());
                let mut r = Json(m).into_response();
                source_headers(&mut r);
                r
            }
        };
        let range_route =
            move |HttpPath((id, member)): HttpPath<(Uuid, String)>,
                  Query(query): Query<std::collections::HashMap<String, u64>>| {
                let m = m.clone();
                let bytes = if member == "manifest" {
                    export.manifest_bytes.clone()
                } else {
                    export.bundle.clone()
                };
                let root = root.clone();
                let mode = range_mode.clone();
                async move {
                    let offset = query["offset"];
                    let limit = query["limit"];
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap())
                            .unwrap();
                    assert_eq!(
                        snapshot.state.attempts[&id]
                            .source
                            .as_ref()
                            .unwrap()
                            .pending
                            .as_ref()
                            .unwrap()
                            .offset,
                        offset
                    );
                    entered.store(true, Ordering::SeqCst);
                    if mode.load(Ordering::SeqCst) == 1 {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            "synthetic lost source read",
                        )
                            .into_response();
                    }
                    if mode.load(Ordering::SeqCst) == 2 {
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    let mut r = bytes
                        [offset as usize..(offset + limit).min(bytes.len() as u64) as usize]
                        .to_vec()
                        .into_response();
                    source_headers(&mut r);
                    r.headers_mut()
                        .insert("content-type", "application/octet-stream".parse().unwrap());
                    r.headers_mut().insert(
                        "aq-source-transfer",
                        URL_SAFE_NO_PAD
                            .encode(serde_json::to_vec(&m).unwrap())
                            .parse()
                            .unwrap(),
                    );
                    r.headers_mut()
                        .insert("aq-source-offset", offset.to_string().parse().unwrap());
                    r
                }
            };
        let material_route = move |Json(request): Json<MaterialFetch>| {
            let lease = material_lease.clone();
            async move {
                assert_eq!(request.lease_id, lease.lease_id);
                let delivery = MaterialDelivery {
                    lease,
                    clock: DeliveryClock::new(1700, 30000).unwrap(),
                };
                let mut response = b"synthetic-key".to_vec().into_response();
                response
                    .headers_mut()
                    .insert("content-type", "application/octet-stream".parse().unwrap());
                response
                    .headers_mut()
                    .insert("cache-control", "no-store".parse().unwrap());
                response.headers_mut().insert(
                    "aq-material-delivery",
                    URL_SAFE_NO_PAD
                        .encode(serde_json::to_vec(&delivery).unwrap())
                        .parse()
                        .unwrap(),
                );
                response
            }
        };
        let router = Router::new()
            .route("/v1/worker/profiled/accept", post(accept_route))
            .route("/v1/worker/profiled/prepare", post(prepare_route))
            .route("/v1/worker/profiled/artifact", post(artifact_route))
            .route("/v1/worker/profiled/material", post(material_route))
            .route("/v1/worker/profiled/source/{id}", get(status_route))
            .route("/v1/worker/profiled/source/{id}/{member}", get(range_route));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = NativeClient::from_config(&crate::config::WorkerConfig {
            server_url: format!("http://{address}"),
            node_id: identity().node_id,
            credential: "synthetic-node-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let preparation = Arc::new(client.accept(&accept, cap, None).await.unwrap());
        journal.record_preparation(id, &preparation).unwrap();
        let store = MaterialStore::open(&materials, identity(), 32).unwrap();
        let pipeline = Arc::new(InputPipeline::new(journal, store, client.clone()));
        Self {
            pipeline: Some(pipeline),
            preparation,
            client,
            reference,
            id,
            root: dir.path().join("journal"),
            _dir: dir,
            task,
            mode,
            range_entered,
            inline_entered,
            inline_release,
            inline_max_active,
            grant_mode,
            grant_window_ms,
        }
    }
    async fn all_inputs(&self) -> Vec<crate::native::VerifiedArtifact> {
        let inputs = self
            .pipeline()
            .artifacts(self.id, self.preparation.clone())
            .await
            .unwrap();
        for lease in &self.preparation.plan().materials {
            self.pipeline()
                .material(self.id, self.preparation.clone(), lease.lease_id)
                .await
                .unwrap();
        }
        inputs
    }
    async fn spec(&self) -> crate::native::VerifiedArtifact {
        self.pipeline()
            .artifact(self.id, self.preparation.clone(), self.reference.clone())
            .await
            .unwrap()
    }
}
fn source_headers(r: &mut Response) {
    r.headers_mut()
        .insert("aq-source-api-version", "1".parse().unwrap());
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
}
fn artifact_headers(r: &mut Response, d: &aq_admission::RetainedArtifact, offset: u64) {
    r.headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    r.headers_mut()
        .insert("aq-admission-api-version", "0.1.7".parse().unwrap());
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r.headers_mut().insert(
        "aq-artifact-retention",
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(d).unwrap())
            .parse()
            .unwrap(),
    );
    r.headers_mut()
        .insert("aq-artifact-offset", offset.to_string().parse().unwrap());
}
struct GitPeer {
    client: GitClient,
    mode: Arc<AtomicU8>,
    calls: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
impl Drop for GitPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap()
        }
    }
}
impl GitPeer {
    fn new(journal: PathBuf, m: TransferManifest) -> Self {
        Self::profile(journal, m, true)
    }
    fn profile(journal: PathBuf, m: TransferManifest, native_inspection: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("git.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::create_dir(dir.path().join("artifacts")).unwrap();
        fs::set_permissions(
            dir.path().join("artifacts"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let wid = Uuid::new_v4();
        let work = dir.path().join("workspaces").join(wid.to_string());
        fs::create_dir_all(&work).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let calls = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let faults = mode.clone();
        let recorded = calls.clone();
        let thread = std::thread::spawn(move || {
            let mut created = Value::Null;
            let mut imported = Value::Null;
            let mut lost = false;
            while !flag.load(Ordering::SeqCst) {
                let (mut conn, _) = match listener.accept() {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                let mut line = String::new();
                BufReader::new(conn.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let operation = req["operation"].as_str().unwrap();
                if operation != "capabilities" {
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(journal.join("journal.json")).unwrap())
                            .unwrap();
                    assert!(
                        snapshot.state.attempts[&m.attempt.attempt_id]
                            .source
                            .as_ref()
                            .unwrap()
                            .operations
                            .values()
                            .any(|p| p.request == req)
                    );
                }
                recorded.lock().unwrap().push(req.clone());
                let result = match operation {
                    "capabilities" => {
                        let mut capabilities = json!({"protocol_version":1,"artifact_format_version":1,"limits":{"artifact":{"max_expanded_bytes":536870912u64,"max_bundle_bytes":67108864u64}},"git":{"checkout_profile":"no_attributes_v1"},"operations":["workspace.inspect","retention.inspect","repo.show","artifact.import","workspace.create","workspace.checkpoint","workspace.retire","artifact.export","retention.release","repo.cache-evict"]});
                        if !native_inspection {
                            capabilities["operations"]
                                .as_array_mut()
                                .unwrap()
                                .retain(|v| v != "workspace.inspect" && v != "retention.inspect");
                        }
                        capabilities
                    }
                    "repo.show" => {
                        json!({"repository_id":m.source.repository_id,"binding_generation":1,"identity":m.source.logical_identity,"mode":"worker"})
                    }
                    "artifact.import" => {
                        if imported.is_null() {
                            let native: GitManifest = serde_json::from_slice(
                                &fs::read(
                                    work.parent()
                                        .unwrap()
                                        .parent()
                                        .unwrap()
                                        .join("artifacts/incoming")
                                        .join(m.artifact_id.to_string())
                                        .join("manifest.json"),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                            imported = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":m.source.repository_id,"binding_generation":1,"transfer_id":m.artifact_id,"source":m.source.base,"retention_handle":m.artifact_id,"manifest_digest":native.canonical_digest().unwrap(),"retention_state":"active","historical":false});
                        }
                        if faults.load(Ordering::SeqCst) == 1 && !lost {
                            lost = true;
                            continue;
                        }
                        imported.clone()
                    }
                    "workspace.create" => {
                        if created.is_null() {
                            created = json!({"workspace_id":wid,"repository_id":m.source.repository_id,"binding_generation":1,"generation":1,"base":m.source.base.hex,"path":work,"lifecycle":"active","allocating_operation":req["operation_id"],"last_checkpoint":null,"retiring_operation":null,"config_digest":"a".repeat(64)});
                        }
                        created.clone()
                    }
                    "retention.inspect" => {
                        json!({"id":m.artifact_id,"repository_id":m.source.repository_id,"binding_generation":1,"generation":1,"state":if faults.load(Ordering::SeqCst)==2{"released"}else{"active"},"refs":[{"name":format!("refs/aq-git/imports/{}",m.artifact_id),"oid":m.source.base.hex}]})
                    }
                    "workspace.inspect" => {
                        json!({"workspace":created,"head":if faults.load(Ordering::SeqCst)==3{json!({"algorithm":"sha1","hex":"b".repeat(40)})}else{serde_json::to_value(&m.source.base).unwrap()},"clean":true})
                    }
                    other => panic!("unexpected native Git action {other}"),
                };
                let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true,"result":result});
                if let Some(id) = req.get("operation_id") {
                    reply["operation_id"] = id.clone();
                    reply["operation_state"] = json!("complete");
                }
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                conn.write_all(&bytes).unwrap();
            }
        });
        let client = GitClient::new(&socket, dir.path()).unwrap();
        Self {
            client,
            mode,
            calls,
            stop,
            thread: Some(thread),
            _dir: dir,
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_native_pipeline_journals_every_external_intent_and_keeps_lost_import_id() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    git.mode.store(1, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let first = h.pipeline().snapshot().await.unwrap();
    let imported = first.attempts[&h.id].source.as_ref().unwrap().operations["import"].clone();
    assert!(imported.result.is_none());
    assert!(
        !first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .operations
            .contains_key("create")
    );
    let spec = h.spec().await;
    let proof = h
        .pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
        .await
        .unwrap();
    assert_eq!(proof.attempt_id(), h.id);
    assert_eq!(proof.manifest().source.base.hex, "a".repeat(40));
    assert!(proof.workspace().path.exists());
    let calls = git.calls.lock().unwrap();
    let imports: Vec<_> = calls
        .iter()
        .filter(|r| r["operation"] == "artifact.import")
        .collect();
    assert_eq!(imports.len(), 2);
    assert_eq!(imports[0], imports[1]);
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["operation"] == "workspace.create")
            .count(),
        1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_receipts_cannot_bypass_fresh_retention_or_head_checks() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
        .await
        .unwrap();
    git.mode.store(2, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    git.mode.store(3, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(
        state.attempts[&h.id].source.as_ref().unwrap().operations["import"]
            .result
            .is_some()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_range_is_exact_pending_and_format_two_is_never_upgraded() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.mode.store(1, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let first = h.pipeline().snapshot().await.unwrap();
    let range = first.attempts[&h.id]
        .source
        .as_ref()
        .unwrap()
        .pending
        .clone()
        .unwrap();
    assert_eq!(range.offset, 0);
    assert_eq!(
        first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .manifest_offset,
        0
    );
    h.mode.store(0, Ordering::SeqCst);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 2);
    let bytes = fs::read(root.join("journal.json")).unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), bytes);
}
fn git_request(
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
    let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"operation_id":id,"repository_id":repo,"expected_binding_generation":generation,"params":params});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        output.status.success() && value["ok"] == true,
        "public Git fixture failed: {value}"
    );
    value["result"].clone()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_native_source_http_import_retention_and_clean_workspace() {
    actual_git_hydration(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_received_initial_grant_retains_actual_workspace_and_material_owners() {
    actual_git_hydration(true).await;
}
async fn actual_git_hydration(retain_grant: bool) {
    use std::process::Command;
    let exe =
        PathBuf::from(std::env::var_os("AQ_GIT_TEST_BINARY").expect("explicit public Git binary"));
    let pin = std::env::var("AQ_GIT_TEST_SHA256").expect("explicit binary SHA256");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&exe).unwrap())),
        pin
    );
    assert_eq!(pin.len(), 64);
    let dir = tempfile::tempdir().unwrap();
    let repository = dir.path().join("repository");
    fs::create_dir(&repository).unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "-q", "--initial-branch=main"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        repository.join("native-input.txt"),
        b"synthetic native source bytes\n",
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args(["add", "native-input.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args([
                "-c",
                "user.name=SyntheticFixture",
                "-c",
                "user.email=fixture@invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "native source base"
            ])
            .status()
            .unwrap()
            .success()
    );
    let hex = String::from_utf8(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let oid = json!({"algorithm":"sha1","hex":hex});
    let rid = Uuid::new_v4();
    let central = dir.path().join("central");
    let worker = dir.path().join("worker");
    git_request(
        &exe,
        &central,
        rid,
        "repo.register",
        json!({"binding":{"identity":"synthetic","mode":"central","binding_generation":1,"remote_url":repository,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}}),
        0,
    );
    git_request(&exe, &central, rid, "repo.sync", json!({}), 1);
    let workspace = git_request(
        &exe,
        &central,
        rid,
        "workspace.create",
        json!({"base":oid}),
        1,
    );
    let checkpoint = git_request(
        &exe,
        &central,
        rid,
        "workspace.checkpoint",
        json!({"workspace_id":workspace["workspace_id"],"expected_generation":workspace["generation"],"expected_head":oid,"quiescent":true}),
        1,
    );
    let exported = git_request(
        &exe,
        &central,
        rid,
        "artifact.export",
        json!({"base":oid,"source":checkpoint["source"],"retention_handle":checkpoint["retention_handle"]}),
        1,
    );
    let artifact = Uuid::parse_str(exported["artifact_handle"].as_str().unwrap()).unwrap();
    let manifest_bytes = fs::read(
        central
            .join("artifacts")
            .join(artifact.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    let bundle = fs::read(
        central
            .join("artifacts")
            .join(artifact.to_string())
            .join("objects.bundle"),
    )
    .unwrap();
    let native: GitManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert!(!native.reference_only && !bundle.is_empty());
    let source = SourceBinding {
        repository_id: rid,
        binding_generation: 1,
        logical_identity: "synthetic".into(),
        base: serde_json::from_value(oid).unwrap(),
    };
    let manifest = TransferManifest {
        transfer_id: artifact,
        artifact_id: artifact,
        role: SourceRole::Base,
        attempt: AttemptRef {
            attempt_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(1),
            generation: 1,
        },
        source: source.clone(),
        checkpoint: source.base,
        resolved_spec_digest: "a".repeat(64),
        manifest_bytes: manifest_bytes.len() as u64,
        manifest_digest: format!("{:x}", Sha256::digest(&manifest_bytes)),
        bundle_bytes: bundle.len() as u64,
        bundle_digest: Some(format!("{:x}", Sha256::digest(&bundle))),
    };
    git_request(
        &exe,
        &worker,
        rid,
        "repo.register",
        json!({"binding":{"identity":"synthetic","mode":"worker","binding_generation":1}}),
        0,
    );
    let service = crate::source::GitService::start(&exe, &pin, &worker).unwrap();
    let h = Inputs::new(Export {
        manifest,
        manifest_bytes,
        bundle,
    })
    .await;
    let spec = h.spec().await;
    let proof = h
        .pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, service.client.clone())
        .await
        .unwrap();
    assert_eq!(proof.workspace().generation, 1);
    assert_eq!(proof.manifest().source.repository_id, rid);
    assert!(
        proof
            .workspace()
            .path
            .starts_with(worker.join("workspaces"))
    );
    assert_eq!(
        fs::read(proof.workspace().path.join("native-input.txt")).unwrap(),
        b"synthetic native source bytes\n"
    );
    let all = h.all_inputs().await;
    let renderer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .startup
        .renderer_qualification
        .clone();
    let joined = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation.clone(), all, proof, renderer)
        .await
        .unwrap();
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .prepared
            .is_some()
    );
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    if retain_grant {
        let offer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .reservation
            .offer
            .clone();
        let observed = h
            .client
            .prepare(joined.request(), &h.preparation, &offer, None)
            .await
            .unwrap();
        let committed = h
            .pipeline()
            .record_initial_grant(joined, observed)
            .await
            .unwrap();
        let crate::native::preparation::GrantCommit::Usable(owner) = committed else {
            panic!("actual Git initial grant expired or changed before commit")
        };
        h.pipeline().check_initial_grant(&owner).await.unwrap();
        let state = h.pipeline().snapshot().await.unwrap();
        assert!(state.attempts[&h.id].initial_grant.is_some());
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        return;
    }
    drop(joined);
    let state = h.pipeline().snapshot().await.unwrap();
    let source = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(source.complete() && source.verified.is_some());
    assert_eq!(source.operations.len(), 5);
    // A fresh second call must inspect the real owner again; a prior receipt
    // cannot silently replace either current retention or the actual clean HEAD.
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, service.client.clone())
        .await
        .unwrap();
    let handle = Uuid::parse_str(
        source.operations["import"].result.as_ref().unwrap()["retention_handle"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let current = service
        .client
        .query(
            source.binding().unwrap(),
            "retention.inspect",
            json!({"handle_id":handle,"limit":1}),
        )
        .unwrap();
    let release = crate::source::GitAction::Release {
        handle_id: handle,
        generation: current["generation"].as_u64().unwrap(),
        refs: serde_json::from_value(current["refs"].clone()).unwrap(),
    }
    .request(source.binding().unwrap(), Uuid::new_v4());
    service.client.exchange(&release).unwrap();
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, service.client.clone())
            .await
            .is_err()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_file_sync_and_cursor_commit_faults_reopen_old_or_matching_new_history() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let m = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), m.clone());
        h.mode.store(1, Ordering::SeqCst);
        let spec = h.spec().await;
        assert!(
            h.pipeline()
                .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
                .await
                .is_err()
        );
        let before = h.pipeline().snapshot().await.unwrap();
        let pending = before.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .pending
            .clone()
            .unwrap();
        let spec = h.spec().await;
        h.pipeline().test_journal_fault(fault).await.unwrap();
        h.mode.store(0, Ordering::SeqCst);
        assert!(
            h.pipeline()
                .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
                .await
                .is_err()
        );
        // File sync preceded the attempted metadata commit. Its exact tail is
        // retained even when the selected journal still has the old cursor.
        let file = git
            .client
            .root()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("download/manifest.json");
        assert_eq!(
            fs::metadata(&file).unwrap().len(),
            u64::from(pending.length)
        );
        drop(h.pipeline.take());
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        assert!(reopened.source_range(h.id).is_err());
        let s = reopened.state().attempts[&h.id].source.as_ref().unwrap();
        assert!(s.operations.is_empty());
        match fault {
            WriteFault::BeforeWrite | WriteFault::AfterFileSync => {
                assert_eq!(s.manifest_offset, 0);
                assert_eq!(
                    s.pending.as_ref().unwrap().operation_id,
                    pending.operation_id
                )
            }
            WriteFault::AfterRename | WriteFault::AfterDirectorySync => {
                assert_eq!(s.manifest_offset, u64::from(pending.length));
                assert!(s.pending.is_none())
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_source_read_allows_control_and_async_cancellation_retains_exact_pending_range() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.mode.store(2, Ordering::SeqCst);
    let spec = h.spec().await;
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation.clone();
    let local = git.client.clone();
    let id = h.id;
    let task =
        tokio::spawn(async move { pipeline.hydrate_source(id, preparation, spec, local).await });
    let before = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let s: State =
                serde_json::from_slice::<Snapshot>(&fs::read(h.root.join("journal.json")).unwrap())
                    .unwrap()
                    .state;
            if h.range_entered.load(Ordering::SeqCst) {
                break s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await
        }
    })
    .await
    .unwrap();
    let record = &before.attempts[&h.id];
    let pending = record
        .source
        .as_ref()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .operation_id;
    let control = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        h.client.accept(
            record.acceptance.as_ref().unwrap(),
            record.reservation.deadline_ns,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drop(control);
    task.abort();
    assert!(matches!(task.await, Err(e) if e.is_cancelled()));
    let after = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(state) => break state,
                Err(error) if error.to_string().contains("storage lane overloaded") => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await
                }
                Err(error) => panic!("unexpected retained owner error: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(after.attempts.len(), 1);
    let s = after.attempts[&h.id].source.as_ref().unwrap();
    assert_eq!(s.pending.as_ref().unwrap().operation_id, pending);
    assert_eq!(s.manifest_offset, 0);
    assert!(s.operations.is_empty());
    h.mode.store(0, Ordering::SeqCst);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
        .await
        .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_source_geometry_refuses_before_any_range_or_git_mutation() {
    let mut export = synthetic();
    export.manifest.bundle_bytes = 512 * 262144;
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let s = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(s.manifest.is_none() && s.pending.is_none() && s.operations.is_empty());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_native_inspection_capabilities_refuse_without_adopting_source_plan() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::profile(h.root.clone(), manifest, false);
    assert!(git.client.native_reservation_bytes().is_err());
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].source.is_none());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_retains_exact_request_and_requires_actual_inputs_again_for_retry() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation.clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    let renderer = before.attempts[&h.id]
        .reservation
        .offer
        .startup
        .renderer_qualification
        .clone();
    let prepared = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation.clone(), all, source, renderer.clone())
        .await
        .unwrap();
    assert_eq!(prepared.request().journal_revision, before.revision);
    assert_eq!(
        prepared.request().journal_digest,
        canonical_digest(&before).unwrap()
    );
    assert_eq!(
        prepared.request().artifacts.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
            + 2
    );
    assert_eq!(
        prepared.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    let saved = h.pipeline().snapshot().await.unwrap();
    assert!(
        same(
            &saved.attempts[&h.id].prepared.as_ref().unwrap().request,
            prepared.request()
        )
        .unwrap()
    );
    let first_request = prepared.request().clone();
    drop(prepared);
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation.clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let retry = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation.clone(), all, source, renderer)
        .await
        .unwrap();
    assert!(same(&first_request, retry.request()).unwrap());
    assert_eq!(retry.request().journal_revision, before.revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_refuses_missing_inputs_renderer_and_retention_changed_after_hydration() {
    for mode in 0..5 {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let mut all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation.clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        let mut renderer = before.attempts[&h.id]
            .reservation
            .offer
            .startup
            .renderer_qualification
            .clone();
        match mode {
            0 => {
                all.pop();
            }
            1 => renderer.digest = Digest::of(b"uninstalled renderer"),
            2 => git.mode.store(2, Ordering::SeqCst),
            3 => git.mode.store(3, Ordering::SeqCst),
            _ => {
                let lease = h.preparation.plan().materials[0].lease_id;
                fs::write(
                    h._dir
                        .path()
                        .join("materials")
                        .join(lease.to_string())
                        .join("material.bin"),
                    b"corrupt installed key",
                )
                .unwrap();
            }
        }
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation.clone(), all, source, renderer)
                .await
                .is_err(),
            "mode {mode}"
        );
        let after = h.pipeline().snapshot().await.unwrap();
        assert!(same(&before, &after).unwrap());
        assert!(after.attempts[&h.id].prepared.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_fsync_cuts_retain_original_or_complete_intent_and_reopen_without_authority() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation.clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        let renderer = before.attempts[&h.id]
            .reservation
            .offer
            .startup
            .renderer_qualification
            .clone();
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation.clone(), all, source, renderer)
                .await
                .is_err()
        );
        assert!(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .reservation
                .disk_bytes
                > 0
        );
        drop(h.pipeline.take());
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        if let Some(p) = &reopened.state().attempts[&h.id].prepared {
            assert_eq!(p.request.journal_revision, before.revision);
            assert_eq!(p.request.journal_digest, canonical_digest(&before).unwrap());
        } else {
            assert!(same(reopened.state(), &before).unwrap());
        }
    }
}

#[test]
fn format_three_refusal_preserves_old_journal_and_pending_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old-three");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 3);
    let original = fs::read(root.join("journal.json")).unwrap();
    fs::write(root.join("pending"), b"unknown old write").unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), original);
    assert_eq!(
        fs::read(root.join("pending")).unwrap(),
        b"unknown old write"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_overlaps_only_two_reads_and_preserves_control_capacity() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation.clone();
    let id = h.id;
    let job = tokio::spawn(async move { pipeline.artifacts(id, preparation).await });
    // Both requests must reach the independently blocked HTTP peer. Sequential
    // fetching cannot satisfy this barrier; no timing benchmark is involved.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.inline_entered.acquire_many(2),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    let pending = h.pipeline().snapshot().await.unwrap();
    assert_eq!(pending.revision, before.revision + 1);
    let attempt = &pending.attempts[&h.id];
    assert!(attempt.downloads.values().all(|d| d.receipt.is_none()));
    // Both existing bulk slots are occupied, but authenticated control still
    // completes on its independent lane. Replaying does not extend the cap.
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.client.accept(
            attempt.acceptance.as_ref().unwrap(),
            attempt.reservation.deadline_ns,
            Some(&h.preparation),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(replay.deadline_ns() <= h.preparation.deadline_ns());
    let requests: usize = attempt.downloads.values().map(|d| d.requests.len()).sum();
    h.inline_release.add_permits(requests);
    let inputs = job.await.unwrap().unwrap();
    assert_eq!(inputs.len(), attempt.downloads.len());
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.pipeline().snapshot().await.unwrap().revision,
        before.revision + 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_uses_two_durable_commits_and_replay_reuses_every_original_request() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    let first = h
        .pipeline()
        .artifacts(h.id, h.preparation.clone())
        .await
        .unwrap();
    let saved = h.pipeline().snapshot().await.unwrap();
    assert_eq!(saved.revision, before.revision + 2);
    assert_eq!(
        first.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    for op in saved.attempts[&h.id].downloads.values() {
        assert_eq!(op.created_revision, before.revision);
        assert!(op.receipt.is_some() && op.completed_ns.is_some());
    }
    let second = h
        .pipeline()
        .artifacts(h.id, h.preparation.clone())
        .await
        .unwrap();
    assert_eq!(second.len(), first.len());
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.bytes(), b.bytes());
    }
    assert!(same(&saved, &h.pipeline().snapshot().await.unwrap()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_inline_batch_keeps_exact_plan_without_partial_receipt_completion() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(3, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .artifacts(h.id, h.preparation.clone())
            .await
            .is_err()
    );
    let partial = h.pipeline().snapshot().await.unwrap();
    assert_eq!(partial.revision, before.revision + 1);
    let plans = &partial.attempts[&h.id].downloads;
    assert_eq!(
        plans.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    assert!(
        plans
            .values()
            .all(|p| p.receipt.is_none() && p.completed_ns.is_none())
    );
    h.mode.store(0, Ordering::SeqCst);
    h.pipeline()
        .artifacts(h.id, h.preparation.clone())
        .await
        .unwrap();
    let complete = h.pipeline().snapshot().await.unwrap();
    assert_eq!(complete.revision, before.revision + 2);
    for (key, p) in plans {
        let actual = &complete.attempts[&h.id].downloads[key];
        assert!(same(&p.requests, &actual.requests).unwrap());
        assert_eq!(p.request_digests, actual.request_digests);
        assert_eq!(p.created_revision, actual.created_revision);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_fsync_cuts_never_reopen_partial_plan_or_completion() {
    for completion in [false, true] {
        for fault in [
            WriteFault::BeforeWrite,
            WriteFault::AfterFileSync,
            WriteFault::AfterRename,
            WriteFault::AfterDirectorySync,
        ] {
            let mut h = Inputs::new(synthetic()).await;
            if completion {
                h.mode.store(3, Ordering::SeqCst);
                assert!(
                    h.pipeline()
                        .artifacts(h.id, h.preparation.clone())
                        .await
                        .is_err()
                );
                h.mode.store(0, Ordering::SeqCst);
            }
            let before = h.pipeline().snapshot().await.unwrap();
            h.pipeline().test_journal_fault(fault).await.unwrap();
            assert!(
                h.pipeline()
                    .artifacts(h.id, h.preparation.clone())
                    .await
                    .is_err()
            );
            drop(h.pipeline.take());
            let mut capacity = limits();
            capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
            let old = Journal::open(&h.root, identity().node_id, capacity).unwrap();
            assert!(old.available().is_err());
            let a = &old.state().attempts[&h.id];
            let count = a.reservation.offer.offered.artifacts.len();
            assert!(a.downloads.is_empty() || a.downloads.len() == count);
            let complete = a.downloads.values().filter(|d| d.receipt.is_some()).count();
            assert!(complete == 0 || complete == count);
            assert!(
                old.state().revision == before.revision
                    || old.state().revision == before.revision + 1
            );
            if completion {
                for (key, op) in &before.attempts[&h.id].downloads {
                    assert!(same(&op.requests, &a.downloads[key].requests).unwrap());
                }
            }
        }
    }
}

#[path = "granted_scenarios.rs"]
mod granted_scenarios;
