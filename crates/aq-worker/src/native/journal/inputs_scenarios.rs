//! Sanitized synthetic public-contract peer; actual private journal/material fs.
use super::*;
use crate::native::{
    http::NativeClient,
    materials::{InstallFault, MaterialStore},
    preparation::InputPipeline,
};
use aq_admission::{
    control::{ControlReply, PreparationDelivery},
    transport::{ArtifactFetch, DeliveryClock, MaterialDelivery, MaterialFetch},
};
use axum::{
    Json, Router,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};
const SECRET: &[u8] = b"synthetic private provider material, never journal";
#[derive(Default)]
struct Options {
    journal_fault: Option<WriteFault>,
    material_fault: Option<InstallFault>,
    pin_fault: bool,
    short: bool,
}
struct Inputs {
    pipeline: Option<Arc<InputPipeline>>,
    client: NativeClient,
    preparation: Arc<Preparation>,
    accept: Accept,
    cap: u64,
    root: std::path::PathBuf,
    materials: std::path::PathBuf,
    id: Uuid,
    lease: Uuid,
    large: ArtifactRef,
    bytes: Vec<u8>,
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
    mode: Arc<AtomicU8>,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Inputs {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Inputs {
    fn pipeline(&self) -> &Arc<InputPipeline> {
        self.pipeline.as_ref().unwrap()
    }
    async fn new(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        let materials = dir.path().join("materials");
        fs::create_dir(&materials).unwrap();
        fs::set_permissions(&materials, fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal = create(&root);
        let mut offered = offer();
        offered.offered.reservation_ttl_ms = 5000;
        let bytes = vec![b'x'; crate::native::MAX_CHUNK_BYTES + 13];
        let large = ArtifactRef {
            id: Text::new("synthetic.large-input").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&bytes),
        };
        let mut descriptor = offered
            .offered
            .artifacts
            .iter()
            .find(|d| d.artifact == offered.startup.payload)
            .unwrap()
            .clone();
        descriptor.artifact = large.clone();
        descriptor.bytes = bytes.len() as u64;
        descriptor.retention_receipt = ArtifactRef {
            id: Text::new("synthetic.large-retention").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic retention only"),
        };
        offered.offered.artifacts.push(descriptor.clone());
        offered.validate().unwrap();
        let id = journal
            .reserve(
                offered,
                qualification(),
                boot_ns().unwrap() + 4_500_000_000,
                1024,
            )
            .unwrap();
        let accept = journal.begin_accept(id, capabilities()).unwrap();
        let cap = journal.state().attempts[&id].reservation.deadline_ns;
        let mut delivery: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        let expires = if options.short { 2100 } else { 5500 };
        delivery.original.clock = DeliveryClock::new(1500, expires).unwrap();
        delivery.delivery.clock = DeliveryClock::new(1600, expires).unwrap();
        delivery.original.materials[0].expires_at_ms = 8000;
        delivery.original.materials[0].material_bytes = SECRET.len() as u64;
        delivery.original.materials[0].material_digest = Digest::of(SECRET);
        let lease = delivery.original.materials[0].clone();
        let lease_id = lease.lease_id;
        let calls = Arc::new(Mutex::new(vec![]));
        let mode = Arc::new(AtomicU8::new(0));
        let artifact_calls = calls.clone();
        let material_calls = calls.clone();
        let artifact_mode = mode.clone();
        let material_mode = mode.clone();
        let artifact_root = root.clone();
        let material_root = root.clone();
        let body = bytes.clone();
        let router = Router::new()
            .route(
                "/v1/worker/profiled/accept",
                post(move |headers: HeaderMap, Json(request): Json<Accept>| {
                    let mut d = delivery.clone();
                    async move {
                        d.delivery.nonce =
                            Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap())
                                .unwrap();
                        d.original.operation_id = request.operation_id;
                        d.original.reservation = request.reservation;
                        d.original.guardian_id = request.guardian_id;
                        d.original.launch_id = request.launch_id;
                        Json(ControlReply::Preparation(d))
                    }
                }),
            )
            .route(
                "/v1/worker/profiled/artifact",
                post(
                    move |headers: HeaderMap, Json(request): Json<ArtifactFetch>| {
                        let root = artifact_root.clone();
                        let calls = artifact_calls.clone();
                        let descriptor = descriptor.clone();
                        let bytes = body.clone();
                        let mode = artifact_mode.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                            let snapshot: Snapshot = serde_json::from_slice(
                                &fs::read(root.join("journal.json")).unwrap(),
                            )
                            .unwrap();
                            assert_eq!(snapshot.digest, canonical_digest(&snapshot.state).unwrap());
                            assert!(
                                snapshot.state.attempts[&request.binding.attempt_id]
                                    .downloads
                                    .values()
                                    .any(|p| p.requests.iter().any(|r| same(r, &request).unwrap()))
                            );
                            calls
                                .lock()
                                .unwrap()
                                .push(serde_json::to_value(&request).unwrap());
                            if mode.load(Ordering::SeqCst) == 1 {
                                return (
                                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                    "synthetic lost read",
                                )
                                    .into_response();
                            }
                            if mode.load(Ordering::SeqCst) == 2 {
                                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                            }
                            let start = request.offset as usize;
                            let end = start + request.length as usize;
                            let mut response: Response = bytes[start..end].to_vec().into_response();
                            response.headers_mut().insert(
                                "content-type",
                                "application/octet-stream".parse().unwrap(),
                            );
                            response
                                .headers_mut()
                                .insert("cache-control", "no-store".parse().unwrap());
                            response.headers_mut().insert(
                                "aq-artifact-retention",
                                URL_SAFE_NO_PAD
                                    .encode(serde_json::to_vec(&descriptor).unwrap())
                                    .parse()
                                    .unwrap(),
                            );
                            response.headers_mut().insert(
                                "aq-artifact-offset",
                                request.offset.to_string().parse().unwrap(),
                            );
                            response
                        }
                    },
                ),
            )
            .route(
                "/v1/worker/profiled/material",
                post(
                    move |headers: HeaderMap, Json(request): Json<MaterialFetch>| {
                        let root = material_root.clone();
                        let calls = material_calls.clone();
                        let lease = lease.clone();
                        let mode = material_mode.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                            let snapshot: Snapshot = serde_json::from_slice(
                                &fs::read(root.join("journal.json")).unwrap(),
                            )
                            .unwrap();
                            let saved = &snapshot.state.attempts[&request.binding.attempt_id]
                                .material_downloads[&request.lease_id];
                            assert!(same(&saved.request, &request).unwrap());
                            calls
                                .lock()
                                .unwrap()
                                .push(serde_json::to_value(&request).unwrap());
                            if mode.load(Ordering::SeqCst) == 1 {
                                return (
                                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                    "synthetic lost read",
                                )
                                    .into_response();
                            }
                            let delivery = MaterialDelivery {
                                clock: DeliveryClock::new(1600, lease.expires_at_ms).unwrap(),
                                lease,
                            };
                            let mut response: Response = SECRET.to_vec().into_response();
                            response.headers_mut().insert(
                                "content-type",
                                "application/octet-stream".parse().unwrap(),
                            );
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
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = NativeClient::from_config(&crate::config::WorkerConfig {
            server_url: format!("http://{addr}"),
            node_id: identity().node_id,
            credential: "synthetic-node-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let preparation = Arc::new(client.accept(&accept, cap, None).await.unwrap());
        journal.record_preparation(id, &preparation).unwrap();
        if options.pin_fault {
            journal.begin_material_download(id, lease_id).unwrap();
            journal.inject_fault(WriteFault::AfterRename);
        } else if let Some(fault) = options.journal_fault {
            journal.inject_fault(fault);
        }
        let mut material_store = MaterialStore::open(&materials, identity(), 8).unwrap();
        if let Some(fault) = options.material_fault {
            material_store.inject_fault(fault);
        }
        let pipeline = Arc::new(InputPipeline::new(journal, material_store, client.clone()));
        Self {
            pipeline: Some(pipeline),
            client,
            preparation,
            accept,
            cap,
            root,
            materials,
            id,
            lease: lease_id,
            large,
            bytes,
            calls,
            mode,
            task,
            _dir: dir,
        }
    }
}
#[tokio::test]
async fn artifact_requests_are_durable_batched_exact_and_retry_keeps_ids() {
    let h = Inputs::new(Options::default()).await;
    let initial = h.pipeline().snapshot().await.unwrap().revision;
    h.mode.store(1, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .artifact(h.id, h.preparation.clone(), h.large.clone())
            .await
            .is_err()
    );
    let pending = h.pipeline().snapshot().await.unwrap();
    let op = pending.attempts[&h.id].downloads.values().next().unwrap();
    assert_eq!(op.requests.len(), 2);
    assert!(op.receipt.is_none());
    assert_eq!(pending.revision, initial + 1);
    h.mode.store(0, Ordering::SeqCst);
    let verified = h
        .pipeline()
        .artifact(h.id, h.preparation.clone(), h.large.clone())
        .await
        .unwrap();
    assert_eq!(verified.bytes(), h.bytes);
    let complete = h.pipeline().snapshot().await.unwrap();
    assert_eq!(complete.revision, initial + 2);
    {
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], calls[1]);
    }
    drop(verified);
    h.mode.store(1, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .artifact(h.id, h.preparation.clone(), h.large.clone())
            .await
            .is_err()
    );
    h.mode.store(0, Ordering::SeqCst);
    let again = h
        .pipeline()
        .artifact(h.id, h.preparation.clone(), h.large.clone())
        .await
        .unwrap();
    assert_eq!(again.receipt().digest, h.large.digest);
    assert_eq!(h.calls.lock().unwrap().len(), 6);
    assert_eq!(h.pipeline().snapshot().await.unwrap().revision, initial + 2);
}
#[tokio::test]
async fn installed_material_pin_is_independent_durable_and_has_no_plaintext() {
    let h = Inputs::new(Options {
        material_fault: Some(InstallFault::AfterPublish),
        ..Default::default()
    })
    .await;
    assert!(
        h.pipeline()
            .material(h.id, h.preparation.clone(), h.lease)
            .await
            .is_err()
    );
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id].material_downloads[&h.lease]
            .receipt()
            .unwrap()
            .is_none()
    );
    let receipt = h
        .pipeline()
        .material(h.id, h.preparation.clone(), h.lease)
        .await
        .unwrap();
    {
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
    }
    let snapshot = h.pipeline().snapshot().await.unwrap();
    assert!(
        same(
            &snapshot.attempts[&h.id].material_downloads[&h.lease]
                .receipt()
                .unwrap()
                .unwrap(),
            &receipt
        )
        .unwrap()
    );
    let raw = fs::read(h.root.join("journal.json")).unwrap();
    assert!(!raw.windows(SECRET.len()).any(|w| w == SECRET));
    assert!(
        !String::from_utf8(raw)
            .unwrap()
            .contains("synthetic-node-secret")
    );
    let installed = h.pipeline().reopen_material(h.id, h.lease).await.unwrap();
    assert_eq!(installed.read_private().unwrap().as_slice(), SECRET);
    let record = h.materials.join(h.lease.to_string()).join("record.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    let sync = value["material_synced_ns"].as_u64().unwrap();
    value["material_synced_ns"] = serde_json::json!(sync - 1);
    value.sort_all_objects();
    fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    let error = h
        .pipeline()
        .reopen_material(h.id, h.lease)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("independent pin"), "{error}");
}
#[tokio::test]
async fn unknown_journal_write_prevents_http_and_reopen_never_reauthorizes() {
    let mut h = Inputs::new(Options {
        journal_fault: Some(WriteFault::AfterRename),
        ..Default::default()
    })
    .await;
    assert!(
        h.pipeline()
            .artifact(h.id, h.preparation.clone(), h.large.clone())
            .await
            .is_err()
    );
    assert!(h.calls.lock().unwrap().is_empty());
    drop(h.pipeline.take());
    let mut recovered = Journal::open(&h.root, identity().node_id, limits()).unwrap();
    assert!(recovered.recovery_required());
    assert_eq!(recovered.state().attempts[&h.id].downloads.len(), 1);
    assert!(recovered.begin_artifact_download(h.id, &h.large).is_err());
    assert_eq!(recovered.state().attempts.len(), 1);
}
#[tokio::test]
async fn postrename_pin_survives_but_is_historical_and_never_recreates_material() {
    let mut h = Inputs::new(Options {
        pin_fault: true,
        ..Default::default()
    })
    .await;
    assert!(
        h.pipeline()
            .material(h.id, h.preparation.clone(), h.lease)
            .await
            .is_err()
    );
    drop(h.pipeline.take());
    let mut recovered = Journal::open(&h.root, identity().node_id, limits()).unwrap();
    assert!(recovered.recovery_required());
    let expected = recovered.state().attempts[&h.id].material_downloads[&h.lease]
        .receipt()
        .unwrap()
        .unwrap();
    assert!(recovered.begin_material_download(h.id, h.lease).is_err());
    let materials = MaterialStore::open(&h.materials, identity(), 8).unwrap();
    assert_eq!(
        materials
            .reopen_pinned(&expected)
            .unwrap()
            .read_private()
            .unwrap()
            .as_slice(),
        SECRET
    );
    assert_eq!(h.calls.lock().unwrap().len(), 1);
}
#[test]
fn old_format_refusal_does_not_rewrite_or_discard_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    drop(create(&root));
    let file = root.join("journal.json");
    let mut old: Snapshot = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    old.state.format = 10;
    old.digest = canonical_digest(&old.state).unwrap();
    let bytes = canonical_bytes(&old).unwrap();
    fs::write(&file, &bytes).unwrap();
    let pending = root.join("journal.pending");
    fs::write(&pending, b"retained old pending").unwrap();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        Journal::open(&root, identity().node_id, limits())
            .err()
            .unwrap()
            .to_string()
            .contains("unsupported native journal format")
    );
    assert_eq!(fs::read(file).unwrap(), bytes);
    assert_eq!(fs::read(pending).unwrap(), b"retained old pending");
}
#[tokio::test]
async fn original_request_associations_and_metadata_bounds_are_rechecked() {
    let h = Inputs::new(Options::default()).await;
    h.pipeline()
        .artifact(h.id, h.preparation.clone(), h.large.clone())
        .await
        .unwrap();
    let original = h.pipeline().snapshot().await.unwrap();
    let key = original.attempts[&h.id]
        .downloads
        .keys()
        .next()
        .unwrap()
        .clone();
    for duplicate in [false, true] {
        let mut state = original.clone();
        let accept_id = state.attempts[&h.id].reservation.accept_operation_id;
        let request = &mut state
            .attempts
            .get_mut(&h.id)
            .unwrap()
            .downloads
            .get_mut(&key)
            .unwrap()
            .requests[0];
        if duplicate {
            request.operation_id = accept_id;
        } else {
            request.offset = 1;
        }
        let digest = canonical_digest(request).unwrap();
        state
            .attempts
            .get_mut(&h.id)
            .unwrap()
            .downloads
            .get_mut(&key)
            .unwrap()
            .request_digests[0] = digest;
        assert!(state.validate(&limits()).is_err());
    }
    let mut state = original.clone();
    let op = state.attempts[&h.id].downloads[&key].clone();
    for i in 0..127 {
        state
            .attempts
            .get_mut(&h.id)
            .unwrap()
            .downloads
            .insert(format!("fake-{i}"), op.clone());
    }
    assert!(
        state
            .validate(&limits())
            .err()
            .unwrap()
            .to_string()
            .contains("operation bound")
    );
}
#[tokio::test]
async fn expired_download_stays_pending_and_bulk_does_not_occupy_control() {
    let h = Inputs::new(Options {
        short: true,
        ..Default::default()
    })
    .await;
    h.mode.store(2, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let observed = h.preparation.clone();
    let id = h.id;
    let reference = h.large.clone();
    let running = tokio::spawn(async move { pipeline.artifact(id, observed, reference).await });
    for _ in 0..100 {
        if !h.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(!h.calls.lock().unwrap().is_empty());
    assert!(
        h.pipeline()
            .material(h.id, h.preparation.clone(), h.lease)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("bulk lane overloaded")
    );
    h.client
        .accept(&h.accept, h.cap, Some(&h.preparation))
        .await
        .unwrap();
    assert!(running.await.unwrap().is_err());
    let before = h.calls.lock().unwrap().len();
    assert!(
        h.pipeline()
            .artifact(h.id, h.preparation.clone(), h.large.clone())
            .await
            .is_err()
    );
    assert_eq!(h.calls.lock().unwrap().len(), before);
    let snapshot = h.pipeline().snapshot().await.unwrap();
    assert!(
        snapshot.attempts[&h.id]
            .downloads
            .values()
            .next()
            .unwrap()
            .receipt
            .is_none()
    );
}
