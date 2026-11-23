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
#[allow(dead_code)]
#[path = "fixtures/journal-v3.rs"]
mod legacy_v3;
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
fn authorize(rt: &mut Runtime<FakeClock>, p: &Peer, o: Offer) {
    let mut coordinator = FakeCoordinator::new(o.clone(), 100);
    let accept = rt.reserve_source(o, &p.client).unwrap();
    rt.authorize(coordinator.accept(&accept).unwrap().grant, 100)
        .unwrap();
}

fn upload_fixture() -> (Peer, TempDir, Runtime<FakeClock>, Uuid) {
    let peer = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    authorize(&mut rt, &peer, o);
    rt.prepare_source(id, &peer.client).unwrap();
    let handle = rt
        .launch_fixture(id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(id, handle).unwrap();
    rt.checkpoint_source(id, &peer.client).unwrap();
    rt.export_source(id, &peer.client).unwrap();
    rt.prepare_source_upload(id, &peer.client).unwrap();
    (peer, temp, rt, id)
}

fn upload_status(rt: &Runtime<FakeClock>, id: Uuid) -> TransferStatus {
    let source = rt.journal.state().attempts[&id].source.as_ref().unwrap();
    let upload = source.upload.as_ref().unwrap();
    TransferStatus {
        transfer_id: source.transfer.as_ref().unwrap().transfer_id,
        manifest_offset: upload.manifest_offset,
        bundle_offset: upload.bundle_offset,
        receipt: None,
    }
}

#[path = "support/source_http.rs"]
mod source_http;
#[path = "support/source_repository.rs"]
mod source_repository;

#[path = "support/source_cli.rs"]
mod source_cli;

include!("source/git_peer.rs");
include!("source/journal_upgrade.rs");
include!("source/download.rs");
include!("source/transfer_upload.rs");
include!("source/admission.rs");
include!("source/registration.rs");
include!("source/pinned_workspace.rs");
include!("source/worker_cli.rs");
include!("source/checkpoint_author.rs");
