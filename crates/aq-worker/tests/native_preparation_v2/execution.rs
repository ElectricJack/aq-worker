use super::*;
use aq_admission::preparation_v2::PrepareRequest;
fn prepare() -> PrepareRequest {
    serde_json::from_value(fixture()["prepare"].clone()).unwrap()
}
#[tokio::test]
async fn execution_outlasts_accepted_preparation_and_retry_preserves_first_observation() {
    let s = Server::new().await;
    let p = s.propose().await;
    let a = s.request();
    s.peer.mode.store(10, Ordering::SeqCst);
    let accepted = s.client.accept_preparation(&p, &a, None).await.unwrap();
    s.peer.mode.store(0, Ordering::SeqCst);
    let q = prepare();
    let first = s
        .client
        .prepare_execution(&p, &a, &accepted, &q, None)
        .await
        .unwrap();
    assert!(first.deadline_ns() > accepted.deadline_ns() + 5_000_000_000);
    assert!(first.hard_deadline_ns() >= first.deadline_ns());
    assert_eq!(first.first_send_deadline_ns(), accepted.deadline_ns());
    assert_eq!(first.request_digest(), &q.digest().unwrap());
    assert_eq!(
        first.accepted_digest(),
        &accepted.delivery().receipt.digest().unwrap()
    );
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    let sends = s.peer.sends.load(Ordering::SeqCst);
    assert!(
        s.client
            .prepare_execution(&p, &a, &accepted, &q, None)
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
    let replay = s
        .client
        .prepare_execution(&p, &a, &accepted, &q, Some(&first))
        .await
        .unwrap();
    assert!(replay.deadline_ns() <= first.deadline_ns());
    assert_eq!(
        replay.first_send_deadline_ns(),
        first.first_send_deadline_ns()
    );
    assert_ne!(replay.first_send_deadline_ns(), first.deadline_ns());
    assert!(replay.hard_deadline_ns() <= first.hard_deadline_ns());
    assert_ne!(
        replay.delivery().delivery.nonce,
        first.delivery().delivery.nonce
    );
    let (original, sent, received, deadline, hard) = replay.first_transmission();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(first.delivery()).unwrap()
    );
    assert_eq!(
        (sent, received, deadline, hard),
        (
            first.sent_ns(),
            first.received_ns(),
            first.deadline_ns(),
            first.hard_deadline_ns()
        )
    );
    let mut changed = q.clone();
    changed.original.operation_id = Uuid::new_v4();
    let sends = s.peer.sends.load(Ordering::SeqCst);
    assert!(
        s.client
            .prepare_execution(&p, &a, &accepted, &changed, Some(&first))
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
    s.peer.mode.store(13, Ordering::SeqCst);
    assert!(
        s.client
            .prepare_execution(&p, &a, &accepted, &q, Some(&first))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn execution_refuses_wrong_nonce_kind_history_late_large_redirect_and_changed_request() {
    let s = Server::new().await;
    let p = s.propose().await;
    let a = s.request();
    let accepted = s.client.accept_preparation(&p, &a, None).await.unwrap();
    let q = prepare();
    for mode in [1, 3, 6, 7, 11, 12, 14] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        let sends = s.peer.sends.load(Ordering::SeqCst);
        assert!(
            s.client
                .prepare_execution(&p, &a, &accepted, &q, None)
                .await
                .is_err(),
            "mode {mode}"
        );
        assert_eq!(
            s.peer.sends.load(Ordering::SeqCst),
            sends + 1,
            "no automatic retry/redirect"
        );
    }
}
#[tokio::test]
async fn expired_execution_observation_never_resurrects_on_retry() {
    let s = Server::new().await;
    let p = s.propose().await;
    let a = s.request();
    let accepted = s.client.accept_preparation(&p, &a, None).await.unwrap();
    s.peer.mode.store(15, Ordering::SeqCst);
    let q = prepare();
    let first = s
        .client
        .prepare_execution(&p, &a, &accepted, &q, None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    s.peer.mode.store(0, Ordering::SeqCst);
    let sends = s.peer.sends.load(Ordering::SeqCst);
    assert!(
        s.client
            .prepare_execution(&p, &a, &accepted, &q, Some(&first))
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
}
#[tokio::test]
async fn canceled_execution_reply_remains_unknown_and_cannot_enable_late_first_send() {
    let s = Server::new().await;
    let p = s.propose().await;
    let a = s.request();
    s.peer.mode.store(10, Ordering::SeqCst);
    let accepted = s.client.accept_preparation(&p, &a, None).await.unwrap();
    s.peer.mode.store(8, Ordering::SeqCst);
    let q = prepare();
    {
        let request = s.client.prepare_execution(&p, &a, &accepted, &q, None);
        tokio::pin!(request);
        tokio::select! {
            _ = &mut request => panic!("blocked reply unexpectedly completed"),
            entry = s.peer.entered.acquire() => entry.unwrap().forget(),
        }
    }
    s.peer.release.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    s.peer.mode.store(0, Ordering::SeqCst);
    let sends = s.peer.sends.load(Ordering::SeqCst);
    assert!(
        s.client
            .prepare_execution(&p, &a, &accepted, &q, None)
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
}
