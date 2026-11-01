use super::*;
use crate::{
    config::WorkerConfig,
    native::journal::{Limits, RuntimeIdentity, WriteFault, WritePause},
};
use aq_session_protocol::Text;
use std::os::unix::fs::PermissionsExt;

fn limits() -> Limits {
    Limits {
        slots: 1,
        memory_bytes: u64::MAX,
        cpu_millis: u64::MAX,
        disk_bytes: 4096,
    }
}
fn runtime() -> RuntimeIdentity {
    let artifact = |name: &str| ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    };
    RuntimeIdentity {
        runtime: artifact("test.runtime"),
        qualification: artifact("test.qualification"),
    }
}
fn setup() -> (
    tempfile::TempDir,
    Arc<InputPipeline>,
    aq_admission::startup::NativeOffer,
    Uuid,
) {
    let dir = tempfile::tempdir().unwrap();
    let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let old: aq_admission::transport::WorkerIdentity =
        serde_json::from_value(fixture["requests"]["accept"]["identity"].clone()).unwrap();
    let journal =
        Journal::create_with_identity_for_test(&dir.path().join("journal"), old.clone(), limits())
            .unwrap();
    let identity = journal.state().identity.clone();
    let offer = serde_json::from_value(fixture["native_offer"].clone()).unwrap();
    let path = dir.path().join("materials");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let materials = MaterialStore::open(&path, identity, 32).unwrap();
    let client = NativeClient::from_config(&WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: old.node_id,
        credential: "test-only".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap();
    (
        dir,
        Arc::new(InputPipeline::new(journal, materials, client)),
        offer,
        old.node_id,
    )
}
#[tokio::test]
async fn new_pipeline_reservation_publishes_membership_and_refuses_duplicate_without_change() {
    let (_dir, pipeline, offer, _) = setup();
    let id = pipeline
        .reserve(
            offer.clone(),
            runtime(),
            boot_ns().unwrap() + 400_000_000,
            1024,
        )
        .await
        .unwrap();
    assert!(pipeline.retained.lock().unwrap().admitted.contains(&id));
    assert_eq!(pipeline.available().await.unwrap().slots, 0);
    let revision = pipeline.snapshot().await.unwrap().revision;
    assert!(
        pipeline
            .reserve(offer, runtime(), boot_ns().unwrap() + 400_000_000, 1024)
            .await
            .is_err()
    );
    assert_eq!(pipeline.snapshot().await.unwrap().revision, revision);
    assert_eq!(pipeline.retained.lock().unwrap().admitted.len(), 1);
}
#[tokio::test]
async fn failed_publication_retains_history_and_requires_recovery_without_membership() {
    let (dir, pipeline, offer, node) = setup();
    pipeline
        .test_journal_fault(WriteFault::AfterRename)
        .await
        .unwrap();
    assert!(
        pipeline
            .reserve(offer, runtime(), boot_ns().unwrap() + 400_000_000, 1024)
            .await
            .is_err()
    );
    assert!(pipeline.available().await.is_err());
    assert!(pipeline.retained.lock().unwrap().admitted.is_empty());
    drop(pipeline);
    let recovered = Journal::open(&dir.path().join("journal"), node, limits()).unwrap();
    assert_eq!(recovered.state().attempts.len(), 1);
    assert!(recovered.recovery_required());
    assert!(recovered.available().is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_waiter_cannot_split_fsync_and_retained_membership() {
    let (_dir, pipeline, offer, _) = setup();
    let pause = WritePause::new();
    pipeline.test_pause_grant_commit(&pause).await.unwrap();
    let other = pipeline.clone();
    let task = tokio::spawn(async move {
        other
            .reserve(offer, runtime(), boot_ns().unwrap() + 400_000_000, 1024)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !pause.entered() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Ok(snapshot) = pipeline.snapshot().await
                && snapshot.attempts.len() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(pipeline.retained.lock().unwrap().admitted.len(), 1);
    assert_eq!(pipeline.available().await.unwrap().slots, 0);
}
