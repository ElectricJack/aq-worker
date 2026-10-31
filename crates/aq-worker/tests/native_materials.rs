#![cfg(feature = "test-support")]
//! Synthetic actual HTTP preparation/material plus actual private filesystem.
use aq_admission::{MaterialLease, control::PreparationDelivery, transport::*};
use aq_session_protocol::Digest;
use aq_worker::{
    config::WorkerConfig,
    native::{
        http::{NativeClient, PrivateMaterial, control::Preparation},
        materials::{InstallFault, MaterialStore},
    },
};
use axum::{
    Json, Router,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::Arc,
};
use uuid::Uuid;
fn fixture() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap()
}
fn boot_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
struct Peer {
    client: NativeClient,
    accept: Accept,
    lease: MaterialLease,
    operation: Uuid,
    task: tokio::task::JoinHandle<()>,
}
impl Peer {
    async fn new(lease_id: Uuid, short: bool) -> Self {
        let accept: Accept =
            serde_json::from_value(fixture()["requests"]["accept"].clone()).unwrap();
        let mut reply: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        let bytes = Arc::new(b"synthetic provider material\n\x00literal".to_vec());
        let lease = &mut reply.original.materials[0];
        lease.lease_id = lease_id;
        lease.material_bytes = bytes.len() as u64;
        lease.material_digest = Digest::of(&bytes);
        let lease = lease.clone();
        let server_lease = lease.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id: accept.identity.node_id,
            credential: "synthetic-machine".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        let router = Router::new()
            .route(
                "/v1/worker/profiled/accept",
                post(move |headers: HeaderMap| {
                    let mut r = reply.clone();
                    async move {
                        r.delivery.nonce =
                            Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap())
                                .unwrap();
                        Json(json!({"kind":"preparation","body":r}))
                    }
                }),
            )
            .route(
                "/v1/worker/profiled/material",
                post(move |Json(request): Json<MaterialFetch>| {
                    let lease = server_lease.clone();
                    let bytes = bytes.clone();
                    async move {
                        assert_eq!(request.lease_id, lease.lease_id);
                        assert_eq!(request.binding, lease.binding);
                        let server_time = if short {
                            lease.expires_at_ms - 100
                        } else {
                            1600
                        };
                        let delivery = MaterialDelivery {
                            lease: lease.clone(),
                            clock: DeliveryClock::new(server_time, lease.expires_at_ms).unwrap(),
                        };
                        let mut response: Response = bytes.to_vec().into_response();
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
                }),
            );
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            client: NativeClient::from_config(&config).unwrap(),
            accept,
            lease,
            operation: Uuid::new_v4(),
            task,
        }
    }
    async fn plan(&self) -> Preparation {
        self.client
            .accept(&self.accept, boot_ns() + 2_000_000_000, None)
            .await
            .unwrap()
    }
    async fn material(&self) -> PrivateMaterial {
        self.client
            .material(
                &MaterialFetch {
                    schema_version: 1,
                    operation_id: self.operation,
                    identity: self.accept.identity.clone(),
                    binding: self.lease.binding.clone(),
                    lease_id: self.lease.lease_id,
                },
                &self.lease,
            )
            .await
            .unwrap()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn private_root() -> tempfile::TempDir {
    let p = tempfile::tempdir().unwrap();
    std::fs::set_permissions(p.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    p
}

#[tokio::test]
async fn installation_is_exact_durable_private_replay_without_secret_in_receipt() {
    let peer = Peer::new(Uuid::new_v4(), false).await;
    let plan = peer.plan().await;
    let root = private_root();
    let mut store = MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).unwrap();
    assert!(MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).is_err());
    let installed = store.install(peer.material().await, &plan).unwrap();
    let receipt = installed.receipt().unwrap();
    receipt.validate_against(&peer.lease, 1600).unwrap();
    assert_eq!(receipt.received_at_ms, 1600);
    assert_eq!(
        receipt.private_binding.id.as_str(),
        format!("aq.private-material/{}", peer.lease.lease_id)
    );
    let bytes = installed.read_private().unwrap();
    assert_eq!(
        bytes.as_slice(),
        b"synthetic provider material\n\x00literal"
    );
    assert!(
        !serde_json::to_string(&receipt)
            .unwrap()
            .contains("synthetic provider material")
    );
    let replay = store.install(peer.material().await, &plan).unwrap();
    assert_eq!(
        serde_json::to_value(replay.receipt().unwrap()).unwrap(),
        serde_json::to_value(&receipt).unwrap()
    );
    drop(store);
    let mut reopened = MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).unwrap();
    assert_eq!(
        serde_json::to_value(
            reopened
                .install(peer.material().await, &plan)
                .unwrap()
                .receipt()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(receipt).unwrap()
    );
}

#[tokio::test]
async fn changed_operations_permissions_links_and_record_bytes_refuse() {
    let mut peer = Peer::new(Uuid::new_v4(), false).await;
    let plan = peer.plan().await;
    let root = private_root();
    let mut store = MaterialStore::open(root.path(), peer.accept.identity.clone(), 2).unwrap();
    let installed = store.install(peer.material().await, &plan).unwrap();
    let old = peer.operation;
    peer.operation = Uuid::new_v4();
    assert!(store.install(peer.material().await, &plan).is_err());
    peer.operation = old;
    let directory = root.path().join(peer.lease.lease_id.to_string());
    let material = directory.join("material.bin");
    std::fs::set_permissions(&material, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(installed.read_private().is_err());
    std::fs::set_permissions(&material, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = directory.join("extra-link");
    std::fs::hard_link(&material, &link).unwrap();
    assert!(installed.read_private().is_err());
    std::fs::remove_file(link).unwrap();
    let saved = std::fs::read(&material).unwrap();
    std::fs::remove_file(&material).unwrap();
    symlink("record.json", &material).unwrap();
    assert!(installed.read_private().is_err());
    std::fs::remove_file(&material).unwrap();
    std::fs::write(&material, saved).unwrap();
    std::fs::set_permissions(&material, std::fs::Permissions::from_mode(0o600)).unwrap();
    let record = directory.join("record.json");
    let mut bytes = std::fs::read(&record).unwrap();
    bytes.push(b'\n');
    std::fs::write(&record, bytes).unwrap();
    assert!(store.install(peer.material().await, &plan).is_err());
}

#[tokio::test]
async fn root_symlinks_boot_mismatch_capacity_and_unknown_staging_refuse() {
    let peer = Peer::new(Uuid::new_v4(), false).await;
    let plan = peer.plan().await;
    let root = private_root();
    let parent = tempfile::tempdir().unwrap();
    let alias = parent.path().join("alias");
    symlink(root.path(), &alias).unwrap();
    assert!(MaterialStore::open(&alias, peer.accept.identity.clone(), 1).is_err());
    let mut wrong = peer.accept.identity.clone();
    wrong.boot_id = Uuid::new_v4();
    let mut store = MaterialStore::open(root.path(), wrong, 1).unwrap();
    assert!(store.install(peer.material().await, &plan).is_err());
    drop(store);
    let mut store = MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).unwrap();
    store.install(peer.material().await, &plan).unwrap();
    let other = Peer::new(Uuid::new_v4(), false).await;
    let other_plan = other.plan().await;
    assert!(store.install(other.material().await, &other_plan).is_err());
    drop(store);
    let second = private_root();
    let mut store = MaterialStore::open(second.path(), peer.accept.identity.clone(), 2).unwrap();
    store.inject_fault(InstallFault::AfterFiles);
    assert!(store.install(peer.material().await, &plan).is_err());
    assert!(
        second
            .path()
            .join(format!("{}.pending", peer.lease.lease_id))
            .exists()
    );
    assert!(store.install(peer.material().await, &plan).is_err());
}

#[tokio::test]
async fn lost_publication_ack_replays_but_post_fsync_expiry_stays_refused() {
    let peer = Peer::new(Uuid::new_v4(), false).await;
    let plan = peer.plan().await;
    let root = private_root();
    let mut store = MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).unwrap();
    store.inject_fault(InstallFault::AfterPublish);
    assert!(store.install(peer.material().await, &plan).is_err());
    assert!(root.path().join(peer.lease.lease_id.to_string()).exists());
    assert!(store.install(peer.material().await, &plan).is_ok());
    let short = Peer::new(Uuid::new_v4(), true).await;
    let plan = short.plan().await;
    let second = tempfile::tempdir().unwrap();
    std::fs::set_permissions(second.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut store = MaterialStore::open(second.path(), short.accept.identity.clone(), 1).unwrap();
    store.inject_fault(InstallFault::PauseAfterFiles);
    assert!(store.install(short.material().await, &plan).is_err());
    assert!(
        second
            .path()
            .join(short.lease.lease_id.to_string())
            .exists(),
        "expiry occurs after durable publication"
    );
    // Even a synthetic peer retiming the same lease cannot revive stored expiry.
    assert!(store.install(short.material().await, &plan).is_err());
}
