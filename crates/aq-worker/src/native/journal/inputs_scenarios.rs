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
#[path = "inputs_scenarios/peer.rs"]
mod peer;
use peer::ReadPeer;
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
    reads: ReadPeer,
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
    async fn fetch_artifact(&self) -> anyhow::Result<crate::native::VerifiedArtifact> {
        self.pipeline()
            .artifact(self.id, self.preparation.clone(), self.large.clone())
            .await
    }
    async fn fetch_material(&self) -> anyhow::Result<aq_admission::MaterialReceipt> {
        self.pipeline()
            .material(self.id, self.preparation.clone(), self.lease)
            .await
    }
    async fn new(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        let materials = dir.path().join("materials");
        fs::create_dir(&materials).unwrap();
        fs::set_permissions(&materials, fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal = create(&root);
        let (offered, descriptor, bytes) = input_offer();
        let large = descriptor.artifact.clone();
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
        let delivery = input_delivery(options.short);
        let lease_id = delivery.original.materials[0].lease_id;
        let reads = ReadPeer::new(root.clone());
        let router = reads.clone().router(delivery, descriptor, bytes.clone());
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
            reads,
            task,
            _dir: dir,
        }
    }
}
fn input_offer() -> (NativeOffer, aq_admission::RetainedArtifact, Vec<u8>) {
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
    (offered, descriptor, bytes)
}
fn input_delivery(short: bool) -> PreparationDelivery {
    let mut delivery: PreparationDelivery =
        serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
    let expires = if short { 2100 } else { 5500 };
    delivery.original.clock = DeliveryClock::new(1500, expires).unwrap();
    delivery.delivery.clock = DeliveryClock::new(1600, expires).unwrap();
    delivery.original.materials[0].expires_at_ms = 8000;
    delivery.original.materials[0].material_bytes = SECRET.len() as u64;
    delivery.original.materials[0].material_digest = Digest::of(SECRET);
    delivery
}
#[tokio::test]
async fn artifact_requests_are_durable_batched_exact_and_retry_keeps_ids() {
    let h = Inputs::new(Options::default()).await;
    let initial = h.pipeline().snapshot().await.unwrap().revision;
    h.reads.mode.store(1, Ordering::SeqCst);
    assert!(h.fetch_artifact().await.is_err());
    let pending = h.pipeline().snapshot().await.unwrap();
    let op = pending.attempts[&h.id].downloads.values().next().unwrap();
    assert_eq!(op.requests.len(), 2);
    assert!(op.receipt.is_none());
    assert_eq!(pending.revision, initial + 1);
    h.reads.mode.store(0, Ordering::SeqCst);
    let verified = h.fetch_artifact().await.unwrap();
    assert_eq!(verified.bytes(), h.bytes);
    let complete = h.pipeline().snapshot().await.unwrap();
    assert_eq!(complete.revision, initial + 2);
    {
        let calls = h.reads.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], calls[1]);
    }
    drop(verified);
    h.reads.mode.store(1, Ordering::SeqCst);
    assert!(h.fetch_artifact().await.is_err());
    h.reads.mode.store(0, Ordering::SeqCst);
    let again = h.fetch_artifact().await.unwrap();
    assert_eq!(again.receipt().digest, h.large.digest);
    assert_eq!(h.reads.calls.lock().unwrap().len(), 6);
    assert_eq!(h.pipeline().snapshot().await.unwrap().revision, initial + 2);
}
#[tokio::test]
async fn installed_material_pin_is_independent_durable_and_has_no_plaintext() {
    let h = Inputs::new(Options {
        material_fault: Some(InstallFault::AfterPublish),
        ..Default::default()
    })
    .await;
    assert!(h.fetch_material().await.is_err());
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id].material_downloads[&h.lease]
            .receipt()
            .unwrap()
            .is_none()
    );
    let receipt = h.fetch_material().await.unwrap();
    {
        let calls = h.reads.calls.lock().unwrap();
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
    assert!(h.fetch_artifact().await.is_err());
    assert!(h.reads.calls.lock().unwrap().is_empty());
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
    assert!(h.fetch_material().await.is_err());
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
    assert_eq!(h.reads.calls.lock().unwrap().len(), 1);
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
    h.fetch_artifact().await.unwrap();
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
        let attempt = state.attempts.get_mut(&h.id).unwrap();
        let download = attempt.downloads.get_mut(&key).unwrap();
        let request = &mut download.requests[0];
        if duplicate {
            request.operation_id = accept_id;
        } else {
            request.offset = 1;
        }
        download.request_digests[0] = canonical_digest(request).unwrap();
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
    let clock = crate::native::http::test_clock::TestClock::new();
    let h = Inputs::new(Options {
        short: true,
        ..Default::default()
    })
    .await;
    h.reads.mode.store(2, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let observed = h.preparation.clone();
    let id = h.id;
    let reference = h.large.clone();
    let running = tokio::spawn(async move { pipeline.artifact(id, observed, reference).await });
    h.reads.entered.notified().await;
    assert!(!h.reads.calls.lock().unwrap().is_empty());
    assert!(
        h.fetch_material()
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
    clock.advance_to(h.preparation.deadline_ns() + 1);
    h.reads.release.notify_one();
    assert!(running.await.unwrap().is_err());
    let before = h.reads.calls.lock().unwrap().len();
    assert!(h.fetch_artifact().await.is_err());
    assert_eq!(h.reads.calls.lock().unwrap().len(), before);
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
