use super::*;
use crate::{
    config::WorkerConfig,
    native::http::{
        NativeClient,
        preparation_refresh_v1::{AcceptedPreparation, ProposedPreparation},
    },
};
use aq_admission::{
    control::DeliveryStamp,
    preparation_refresh_v1::{
        AcceptRequest, AcceptedDelivery, AcceptedReceipt, Document, Proposal,
    },
    transport::DeliveryClock,
};
use axum::{Json, Router, extract::State as HttpState, http::HeaderMap, routing::post};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn phase_fixture() -> serde_json::Value {
    serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.10/artifacts/preparation-refresh-v1.json"
    ))
    .map(|v: serde_json::Value| v["fixture"].clone())
    .unwrap()
}
fn proposal() -> Proposal {
    serde_json::from_value(phase_fixture()["proposal"].clone()).unwrap()
}
fn phase_limits() -> Limits {
    Limits {
        slots: 2,
        memory_bytes: u64::MAX,
        cpu_millis: u64::MAX,
        disk_bytes: 64 * 1024 * 1024 * 1024,
    }
}
fn profile() -> ArtifactRef {
    serde_json::from_value(phase_fixture()["accept"]["hold"]["source_profile"].clone()).unwrap()
}
struct Peer {
    task: tokio::task::JoinHandle<()>,
    remaining: Arc<AtomicU64>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn peer() -> (NativeClient, Peer) {
    async fn propose() -> Json<Document> {
        Json(Document::Proposal(Box::new(proposal())))
    }
    async fn accept(
        HttpState(remaining): HttpState<Arc<AtomicU64>>,
        headers: HeaderMap,
        Json(doc): Json<Document>,
    ) -> Json<Document> {
        let Document::Accept(q) = doc else {
            panic!("expected phase Accept")
        };
        q.validate_for(&proposal()).unwrap();
        let mut receipt: AcceptedReceipt =
            serde_json::from_value(phase_fixture()["accepted"].clone()).unwrap();
        receipt.operation_id = q.original.operation_id;
        receipt.request_digest = q.digest().unwrap();
        receipt.lease.guardian_id = q.original.guardian_id;
        receipt.lease.launch_id = q.original.launch_id;
        receipt.lease.hold = q.hold.clone();
        let nonce = Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
        let end = receipt.lease.expires_at_ms;
        let delivery = AcceptedDelivery {
            schema_version: 2,
            receipt,
            delivery: DeliveryStamp {
                nonce,
                clock: DeliveryClock::new(end - remaining.load(Ordering::SeqCst), end).unwrap(),
            },
        };
        delivery.validate_for(&q, &proposal(), nonce).unwrap();
        Json(Document::AcceptedDelivery(Box::new(delivery)))
    }
    let remaining = Arc::new(AtomicU64::new(30000));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = NativeClient::from_config(&WorkerConfig {
        server_url: format!("http://{}", listener.local_addr().unwrap()),
        node_id: identity().node_id,
        credential: "synthetic-phase-journal".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap();
    let app = Router::new()
        .route("/v1/worker/profiled-v2/proposal", post(propose))
        .route("/v1/worker/profiled-v2/accept", post(accept))
        .with_state(remaining.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (client, Peer { task, remaining })
}
async fn reserved() -> (
    tempfile::TempDir,
    Journal,
    Uuid,
    NativeClient,
    Peer,
    ProposedPreparation,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        Journal::create_identity(&dir.path().join("native"), identity(), phase_limits()).unwrap();
    let p = proposal();
    let disk = phase_fixture()["accept"]["hold"]["source_disk_bytes"]
        .as_u64()
        .unwrap();
    let cap = boot_ns().unwrap() + 400_000_000;
    let id = journal
        .reserve(p.original.clone(), qualification(), cap, disk)
        .unwrap();
    let (client, peer) = peer().await;
    let proposed = client
        .propose_refresh_preparation(&identity(), &p.original, &p.negotiation, cap)
        .await
        .unwrap();
    (dir, journal, id, client, peer, proposed)
}
fn begin(journal: &mut Journal, id: Uuid, p: &ProposedPreparation) -> AcceptRequest {
    journal
        .begin_refresh_accept(id, p, capabilities(), profile(), 30000)
        .unwrap()
}

#[tokio::test]
async fn phase_intent_and_receipt_are_atomic_exact_and_cannot_enter_legacy_paths() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let (dir, mut journal, id, client, _peer, p) = reserved().await;
    let q = begin(&mut journal, id, &p);
    let revision = journal.state().revision;
    assert_eq!(
        canonical_bytes(&q).unwrap(),
        canonical_bytes(&begin(&mut journal, id, &p)).unwrap()
    );
    assert_eq!(journal.state().revision, revision);
    assert!(journal.begin_accept(id, capabilities()).is_err());
    let observed = client
        .accept_refresh_preparation(&p, &q, None)
        .await
        .unwrap();
    assert!(journal.check_refresh_preparation(id, &observed).is_err());
    let first = journal.record_refresh_preparation(id, &observed).unwrap();
    let cap = journal.check_refresh_preparation(id, &observed).unwrap();
    assert!(cap > journal.state().attempts[&id].reservation.deadline_ns);
    assert!(journal.state().attempts[&id].preparation.is_none());
    let revision = journal.state().revision;
    assert_eq!(first.record_revision, revision);
    crate::native::http::test_clock::expire(p.original_deadline_ns()).await;
    let retry = begin(&mut journal, id, &p);
    assert_eq!(
        canonical_bytes(&retry).unwrap(),
        canonical_bytes(&q).unwrap()
    );
    let again = client
        .accept_refresh_preparation(&p, &retry, Some(&observed))
        .await
        .unwrap();
    let history = journal.record_refresh_preparation(id, &again).unwrap();
    assert_eq!(
        canonical_bytes(&first).unwrap(),
        canonical_bytes(&history).unwrap()
    );
    assert_eq!(journal.state().revision, revision);
    assert!(journal.check_refresh_preparation(id, &again).unwrap() <= cap);
    let free = journal.available().unwrap();
    assert_eq!(free.slots, 1);
    drop(journal);
    let recovered = Journal::open(
        &dir.path().join("native"),
        identity().node_id,
        phase_limits(),
    )
    .unwrap();
    assert!(recovered.recovery_required());
    assert!(recovered.available().is_err());
    assert!(recovered.check_refresh_preparation(id, &again).is_err());
    assert_eq!(
        canonical_bytes(
            recovered.state().attempts[&id]
                .refresh_preparation
                .as_ref()
                .unwrap()
        )
        .unwrap(),
        canonical_bytes(&first).unwrap()
    );
}

#[tokio::test]
async fn replay_before_first_persistence_retains_original_transmission_after_proposal_expiry() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let (_dir, mut journal, id, client, _peer, p) = reserved().await;
    let q = begin(&mut journal, id, &p);
    let first = client
        .accept_refresh_preparation(&p, &q, None)
        .await
        .unwrap();
    crate::native::http::test_clock::expire(p.original_deadline_ns()).await;
    let again = client
        .accept_refresh_preparation(&p, &q, Some(&first))
        .await
        .unwrap();
    assert!(again.sent_ns() > p.original_deadline_ns());
    let record = journal.record_refresh_preparation(id, &again).unwrap();
    assert_eq!(record.sent_ns, first.sent_ns());
    assert_eq!(record.received_ns, first.received_ns());
    assert_eq!(
        canonical_bytes(&record.delivery).unwrap(),
        canonical_bytes(first.delivery()).unwrap()
    );
    assert!(journal.check_refresh_preparation(id, &again).is_ok());
}

#[tokio::test]
async fn expiry_before_recording_keeps_received_history_and_every_resource_hold() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let (_dir, mut journal, id, client, peer, p) = reserved().await;
    let q = begin(&mut journal, id, &p);
    peer.remaining.store(100, Ordering::SeqCst);
    let observed = client
        .accept_refresh_preparation(&p, &q, None)
        .await
        .unwrap();
    let held = journal.available().unwrap();
    clock.advance_to(observed.deadline_ns());
    journal.record_refresh_preparation(id, &observed).unwrap();
    assert!(journal.check_refresh_preparation(id, &observed).is_err());
    assert_eq!(
        canonical_bytes(&held).unwrap(),
        canonical_bytes(&journal.available().unwrap()).unwrap()
    );
    assert!(journal.state().attempts[&id].refresh_preparation.is_some());
}

#[tokio::test]
async fn intent_and_receipt_fsync_cuts_keep_old_or_whole_new_protocol_history() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    for receipt in [false, true] {
        for (fault, published) in [
            (WriteFault::BeforeWrite, false),
            (WriteFault::AfterFileSync, false),
            (WriteFault::AfterRename, true),
            (WriteFault::AfterDirectorySync, true),
        ] {
            let (dir, mut journal, id, client, _peer, p) = reserved().await;
            let observed: Option<AcceptedPreparation> = if receipt {
                let q = begin(&mut journal, id, &p);
                Some(
                    client
                        .accept_refresh_preparation(&p, &q, None)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            journal.inject_fault(fault);
            if let Some(observed) = &observed {
                assert!(journal.record_refresh_preparation(id, observed).is_err());
            } else {
                assert!(
                    journal
                        .begin_refresh_accept(id, &p, capabilities(), profile(), 30000)
                        .is_err()
                );
            }
            assert!(journal.recovery_required());
            drop(journal);
            let recovered = Journal::open(
                &dir.path().join("native"),
                identity().node_id,
                phase_limits(),
            )
            .unwrap();
            let a = &recovered.state().attempts[&id];
            if receipt {
                assert_eq!(a.refresh_preparation.is_some(), published);
                assert!(a.refresh_acceptance.is_some());
            } else {
                assert_eq!(a.refresh_acceptance.is_some(), published);
                assert_eq!(a.acceptance.is_some(), published);
            }
            assert!(recovered.recovery_required());
            assert!(recovered.available().is_err());
            assert_eq!(recovered.state().attempts.len(), 1);
        }
    }
}

#[tokio::test]
async fn changed_phase_intent_or_receipt_cannot_replace_first_history() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let (_dir, mut journal, id, client, _peer, p) = reserved().await;
    let q = begin(&mut journal, id, &p);
    let revision = journal.state().revision;
    assert!(
        journal
            .begin_refresh_accept(id, &p, capabilities(), profile(), 20000)
            .is_err()
    );
    let mut changed_profile = profile();
    changed_profile.digest = Digest::of(b"changed");
    assert!(
        journal
            .begin_refresh_accept(id, &p, capabilities(), changed_profile, 30000)
            .is_err()
    );
    assert_eq!(journal.state().revision, revision);
    let mut changed = q.clone();
    changed.original.operation_id = Uuid::new_v4();
    let unrelated = client
        .accept_refresh_preparation(&p, &changed, None)
        .await
        .unwrap();
    assert!(journal.record_refresh_preparation(id, &unrelated).is_err());
    assert_eq!(journal.state().revision, revision);
    let actual = client
        .accept_refresh_preparation(&p, &q, None)
        .await
        .unwrap();
    journal.record_refresh_preparation(id, &actual).unwrap();
    let revision = journal.state().revision;
    assert!(journal.record_refresh_preparation(id, &unrelated).is_err());
    assert_eq!(journal.state().revision, revision);
}

#[tokio::test]
async fn valid_checksum_cannot_hide_phase_association_or_clock_corruption() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    for mode in 0..5 {
        let (dir, mut journal, id, client, _peer, p) = reserved().await;
        let q = begin(&mut journal, id, &p);
        let observed = client
            .accept_refresh_preparation(&p, &q, None)
            .await
            .unwrap();
        journal.record_refresh_preparation(id, &observed).unwrap();
        drop(journal);
        rewrite(&dir.path().join("native"), |s| {
            let a = s.attempts.get_mut(&id).unwrap();
            match mode {
                0 => a.refresh_acceptance.as_mut().unwrap().request_digest = Digest::of(b"wrong"),
                1 => a.refresh_preparation.as_mut().unwrap().deadline_ns = u64::MAX,
                2 => a.refresh_preparation.as_mut().unwrap().record_revision = u64::MAX,
                3 => a.refresh_acceptance = None,
                _ => {
                    a.refresh_acceptance
                        .as_mut()
                        .unwrap()
                        .request
                        .hold
                        .source_disk_bytes = 1
                }
            }
        });
        assert!(
            Journal::open(
                &dir.path().join("native"),
                identity().node_id,
                phase_limits()
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn legacy_protocol_choice_cannot_be_switched_to_phase_after_publication() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let (_dir, mut journal, id, _client, _peer, p) = reserved().await;
    journal.begin_accept(id, capabilities()).unwrap();
    let revision = journal.state().revision;
    assert!(
        journal
            .begin_refresh_accept(id, &p, capabilities(), profile(), 30000)
            .is_err()
    );
    assert_eq!(journal.state().revision, revision);
    assert!(journal.state().attempts[&id].refresh_acceptance.is_none());
}

#[tokio::test]
async fn actual_poll_redelivery_and_original_proposal_are_retained_as_distinct_history() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        Journal::create_identity(&dir.path().join("native"), identity(), phase_limits()).unwrap();
    let p = proposal();
    let mut delivered = p.original.clone();
    delivered.offered.clock = DeliveryClock::new(1600, 2000).unwrap();
    let cap = boot_ns().unwrap() + 350_000_000;
    let disk = phase_fixture()["accept"]["hold"]["source_disk_bytes"]
        .as_u64()
        .unwrap();
    let id = journal
        .reserve(delivered.clone(), qualification(), cap, disk)
        .unwrap();
    let (client, _peer) = peer().await;
    let proposed = client
        .propose_refresh_preparation(&identity(), &delivered, &p.negotiation, cap)
        .await
        .unwrap();
    let q = begin(&mut journal, id, &proposed);
    let observed = client
        .accept_refresh_preparation(&proposed, &q, None)
        .await
        .unwrap();
    journal.record_refresh_preparation(id, &observed).unwrap();
    let a = &journal.state().attempts[&id];
    assert_eq!(a.reservation.offer.offered.clock.server_time_ms, 1600);
    assert_eq!(
        a.refresh_acceptance
            .as_ref()
            .unwrap()
            .proposal
            .original
            .offered
            .clock
            .server_time_ms,
        1500
    );
    assert!(journal.check_refresh_preparation(id, &observed).is_ok());
    drop(journal);
    let history = Journal::open(
        &dir.path().join("native"),
        identity().node_id,
        phase_limits(),
    )
    .unwrap();
    assert!(history.recovery_required());
}

#[tokio::test]
async fn frozen_profile_client_refuses_refresh_reply_on_same_uri_without_fallback() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let (_dir, _journal, _id, client, _peer, p) = reserved().await;
    let old: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
    ))
    .unwrap();
    let old_proposal: aq_admission::preparation_v2::Proposal =
        serde_json::from_value(old["proposal"].clone()).unwrap();
    assert!(
        client
            .propose_preparation(
                &identity(),
                &old_proposal.original,
                &old_proposal.negotiation,
                p.original_deadline_ns()
            )
            .await
            .is_err()
    );
}
