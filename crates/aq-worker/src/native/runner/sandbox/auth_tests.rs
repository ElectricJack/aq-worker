//! A missing bearer/scope check is observable at this recording HTTP peer.
use super::auth::*;
use super::private::RunnerFiles;
use aq_admission_sandbox::{control::DeliveryStamp, sandbox::*};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    routing::any,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};
use uuid::Uuid;

type Recorded = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
struct Peer {
    origin: String,
    seen: Recorded,
    status: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn peer() -> Peer {
    let seen = Recorded::default();
    let status = Arc::new(AtomicU16::new(200));
    async fn record(
        State((seen, status)): State<(Recorded, Arc<AtomicU16>)>,
        uri: Uri,
        headers: HeaderMap,
        bytes: Bytes,
    ) -> (StatusCode, &'static str) {
        seen.lock()
            .unwrap()
            .push((uri.to_string(), headers, bytes.to_vec()));
        (
            StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap(),
            "{}",
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(any(record))
        .with_state((seen.clone(), status.clone()));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Peer {
        origin,
        seen,
        status,
        task,
    }
}
fn fixture(name: &str) -> Value {
    let values: Value = serde_json::from_slice(include_bytes!(
        "../../../../../../protocol/aq-admission/artifacts/sandbox-v1.fixtures.json"
    ))
    .unwrap();
    values
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()["case"]
        .clone()
}
fn stamp(now: u64, expires: u64) -> DeliveryStamp {
    serde_json::from_value(json!({"nonce":Uuid::new_v4(),"clock":{"server_time_ms":now,"expires_at_ms":expires,"valid_for_ms":expires-now}})).unwrap()
}
fn client(origin: &str, prep: bool) -> (tempfile::TempDir, RunnerHttp, DelegatedPermit) {
    let f = fixture(if prep {
        "phase_v2_upgrade"
    } else {
        "phase_v2_handoff"
    });
    let permit: DelegatedPermit =
        serde_json::from_value(f[if prep { "preparation_permit" } else { "permit" }].clone())
            .unwrap();
    let authority = if prep {
        RunnerAuthority::Preparation(serde_json::from_value(f["preparation"].clone()).unwrap())
    } else {
        RunnerAuthority::Execution(serde_json::from_value(f["authority"].clone()).unwrap())
    };
    let dir = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&dir.path().join("private")).unwrap();
    files
        .write_secret("permit.json", &serde_json::to_vec(&permit).unwrap())
        .unwrap();
    let now = super::boot_ns().unwrap();
    let handoff = RunnerHandoff {
        authority,
        binding: permit.binding.clone(),
        delivery: stamp(2000, permit.expires_at_ms),
        sent_boot_ns: now,
        received_boot_ns: now,
        kernel_boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .into(),
    };
    let http = RunnerHttp::from_file(origin, &files.root().join("permit.json"), handoff).unwrap();
    (dir, http, permit)
}
#[tokio::test]
async fn runner_authenticates_with_delegated_permit_and_refuses_foreign_scope() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, permit) = client(&peer.origin, false);
    let b = &permit.binding;
    let body = json!({"binding":b});
    let routes = [
        (RunnerRoute::Material, "/v1/worker/profiled/material"),
        (RunnerRoute::Started, "/v1/worker/profiled-v2/started"),
        (RunnerRoute::Renew, "/v1/worker/profiled/renew"),
        (RunnerRoute::Observations, "/v1/worker/observations/batch"),
        (RunnerRoute::Artifact, "/v1/worker/profiled/artifact"),
        (RunnerRoute::Results, "/v1/worker/profiled/results"),
        (RunnerRoute::Stopped, "/v1/worker/profiled/stopped"),
        (RunnerRoute::MailPoll, "/v2/worker/messages/poll"),
        (RunnerRoute::MailAck, "/v2/worker/messages/ack"),
        (
            RunnerRoute::Terminal(TerminalRoute::Register),
            "/v1/terminal-relay/register",
        ),
        (
            RunnerRoute::Terminal(TerminalRoute::Poll),
            "/v1/terminal-relay/poll",
        ),
        (
            RunnerRoute::Terminal(TerminalRoute::OutputPoll),
            "/v1/terminal-relay/output-poll",
        ),
        (
            RunnerRoute::Terminal(TerminalRoute::ControlUpload),
            "/v1/terminal-relay/control-upload",
        ),
        (
            RunnerRoute::Terminal(TerminalRoute::OutputUpload),
            "/v1/terminal-relay/upload",
        ),
    ];
    for (route, path) in routes {
        http.post(route, b, &body).await.unwrap();
        assert_eq!(peer.seen.lock().unwrap().last().unwrap().0, path);
    }
    http.source(b, SourceMember::Bundle, 0, 1024).await.unwrap();
    assert_eq!(
        peer.seen.lock().unwrap().last().unwrap().0,
        "/v1/worker/profiled/source/00000000-0000-0000-0000-000000000001/bundle?offset=0&limit=1024"
    );
    let (_prep_dir, prep, prep_permit) = client(&peer.origin, true);
    for route in [
        RunnerRoute::Material,
        RunnerRoute::Artifact,
        RunnerRoute::Prepared,
    ] {
        prep.post(
            route,
            &prep_permit.binding,
            &json!({"binding":prep_permit.binding}),
        )
        .await
        .unwrap();
    }
    prep.source(&prep_permit.binding, SourceMember::Manifest, 0, 1024)
        .await
        .unwrap();
    for (_, headers, _) in peer.seen.lock().unwrap().iter() {
        let expected = if headers["authorization"]
            == "Bearer fixture_only_not_a_real_secret_preparation_0000"
        {
            "Bearer fixture_only_not_a_real_secret_preparation_0000"
        } else {
            "Bearer fixture_only_not_a_real_secret_00000000"
        };
        assert_eq!(headers["authorization"], expected);
        assert!(
            !headers.contains_key("cf-access-client-id")
                && !headers.contains_key("cf-access-client-secret")
        );
    }
    let count = peer.seen.lock().unwrap().len();
    for route in [
        RunnerRoute::Started,
        RunnerRoute::Renew,
        RunnerRoute::Observations,
        RunnerRoute::Terminal(TerminalRoute::Poll),
        RunnerRoute::Results,
        RunnerRoute::Stopped,
        RunnerRoute::MailPoll,
        RunnerRoute::MailAck,
    ] {
        assert!(
            prep.post(route, &prep_permit.binding, &json!({}))
                .await
                .is_err()
        );
    }
    let mut foreign = b.clone();
    foreign.attempt_id = Uuid::new_v4();
    assert!(
        http.post(RunnerRoute::Started, &foreign, &body)
            .await
            .is_err()
    );
    assert!(
        http.source(&foreign, SourceMember::Bundle, 0, 1024)
            .await
            .is_err()
    );
    foreign = b.clone();
    foreign.node.boot_id = Uuid::new_v4();
    assert!(
        http.post(RunnerRoute::Started, &foreign, &body)
            .await
            .is_err()
    );
    assert!(
        http.post(
            RunnerRoute::Started,
            b,
            &json!({"nested":[{"attempt_id":Uuid::new_v4()}]})
        )
        .await
        .is_err()
    );
    assert!(
        http.post(
            RunnerRoute::Started,
            b,
            &json!({"session":{"boot_id":Uuid::new_v4()}})
        )
        .await
        .is_err()
    );
    clock.advance_to(super::boot_ns().unwrap() + 5_000_000_000);
    assert!(http.post(RunnerRoute::Started, b, &body).await.is_err());
    assert!(http.source(b, SourceMember::Bundle, 0, 1024).await.is_err());
    assert_eq!(
        peer.seen.lock().unwrap().len(),
        count,
        "foreign/expired/premature scope must not reach server"
    );
}

#[tokio::test]
async fn runner_refuses_foreign_session_on_mail_and_terminal_routes() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, permit) = client(&peer.origin, false);
    let f = fixture("phase_v2_handoff");
    let session = f["authority"]["grant"]["binding"]["session"]["session_id"].clone();
    assert!(session.is_string());
    for route in [
        RunnerRoute::MailPoll,
        RunnerRoute::MailAck,
        RunnerRoute::Terminal(TerminalRoute::Poll),
    ] {
        http.post(route, &permit.binding, &json!({"session_id":session}))
            .await
            .unwrap();
        let count = peer.seen.lock().unwrap().len();
        assert!(
            http.post(
                route,
                &permit.binding,
                &json!({"recipient":{"session_id":Uuid::new_v4()}})
            )
            .await
            .is_err()
        );
        assert_eq!(peer.seen.lock().unwrap().len(), count);
    }
}

#[tokio::test]
async fn runner_channel_refusal_is_shared_authority_loss() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, permit) = client(&peer.origin, false);
    let other = http.clone();
    peer.status.store(403, Ordering::SeqCst);
    assert!(
        http.post(RunnerRoute::MailPoll, &permit.binding, &json!({}))
            .await
            .is_err()
    );
    assert!(other.authority_lost());
    peer.status.store(200, Ordering::SeqCst);
    assert!(
        other
            .post(RunnerRoute::Renew, &permit.binding, &json!({}))
            .await
            .is_err()
    );
    assert_eq!(peer.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn runner_grant_upgrade_and_renewal_keep_original_scope() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, permit) = client(&peer.origin, true);
    let origin = super::boot_ns().unwrap();
    let f = fixture("phase_v2_upgrade");
    let request: PermitUpgradeRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let response: PermitUpgradeResponse = serde_json::from_value(f["response"].clone()).unwrap();
    clock.advance_to(origin + 28_000_000_000); // fixture server time 30000
    let now = super::boot_ns().unwrap();
    let mut foreign = response.clone();
    foreign.permit.binding.node.boot_id = Uuid::new_v4();
    assert!(
        http.install_grant(&request, &foreign, stamp(30000, 50000), now, now)
            .is_err()
    );
    assert!(
        http.post(RunnerRoute::Started, &permit.binding, &json!({}))
            .await
            .is_err()
    );
    http.install_grant(&request, &response, stamp(30000, 50000), now, now)
        .unwrap();
    http.post(RunnerRoute::Started, &permit.binding, &json!({}))
        .await
        .unwrap();
    assert_eq!(
        peer.seen.lock().unwrap()[0].1["authorization"],
        "Bearer fixture_only_not_a_real_secret_execution_00000"
    );
    assert!(
        http.install_grant(&request, &response, stamp(30000, 50000), now, now)
            .is_err()
    );

    let f = fixture("phase_v2_renewal");
    let (_dir, http, _permit) = client(&peer.origin, false);
    let request: RunnerRenewalRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let delivery = super::RenewalDelivery {
        response: serde_json::from_value(f["response"].clone()).unwrap(),
        context: serde_json::from_value(f["context"].clone()).unwrap(),
        server_now_ms: 2000,
    };
    let nonce = serde_json::from_value(f["nonce"].clone()).unwrap();
    let now = super::boot_ns().unwrap();
    http.accept_renewal(&request, &delivery, nonce, now, now)
        .unwrap();
    clock.advance_to(now + 1_100_000_000); // past old expiry, before renewed expiry
    http.post(RunnerRoute::Started, &request.permit.binding, &json!({}))
        .await
        .unwrap();
    assert!(
        http.accept_renewal(&request, &delivery, nonce, now, now)
            .is_err(),
        "replayed old request cannot reinstall authority"
    );
    clock.advance_to(now + 3_000_000_000); // original hard deadline
    assert!(
        http.post(RunnerRoute::Started, &request.permit.binding, &json!({}))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn runner_readoption_replies_extend_nothing() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, _) = client(&peer.origin, false);
    let f = fixture("renewal_awaiting_readoption");
    let request: RunnerRenewalRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let nonce = serde_json::from_value(f["nonce"].clone()).unwrap();
    let start = super::boot_ns().unwrap();
    for millis in [0, 400, 800] {
        clock.advance_to(start + millis * 1_000_000);
        let now = super::boot_ns().unwrap();
        http.post(RunnerRoute::Renew, &request.permit.binding, &request)
            .await
            .unwrap();
        let delivery = super::RenewalDelivery {
            response: serde_json::from_value(f["response"].clone()).unwrap(),
            context: serde_json::from_value(f["context"].clone()).unwrap(),
            server_now_ms: 2000 + millis,
        };
        http.accept_renewal(&request, &delivery, nonce, now, now)
            .unwrap();
    }
    clock.advance_to(start + 1_000_000_000);
    assert!(
        http.post(RunnerRoute::Renew, &request.permit.binding, &request)
            .await
            .is_err()
    );
    assert!(http.authority_lost());
    assert_eq!(peer.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn runner_exchange_refusal_and_unavailability_have_distinct_authority() {
    use super::{
        FinishedReceipt, RenewalOutcome, RetainedAttempt, RetentionReceipts, RunnerTransport,
        StoppedAttempt, TransportFuture,
        permit_transport::{PermitTransport, RunnerExchange},
    };
    struct Exchange;
    impl RunnerExchange for Exchange {
        fn preflight(&mut self, _: &RunnerHttp) -> anyhow::Result<()> {
            Ok(())
        }
        fn renew<'a>(
            &'a mut self,
            http: &'a RunnerHttp,
            request: &'a RunnerRenewalRequest,
            _: Uuid,
        ) -> TransportFuture<'a, RenewalOutcome> {
            Box::pin(async move {
                http.post(RunnerRoute::Renew, &request.permit.binding, request)
                    .await?;
                Ok(RenewalOutcome::Unavailable)
            })
        }
        fn retain<'a>(
            &'a mut self,
            _: &'a RunnerHttp,
            _: &'a StoppedAttempt,
        ) -> TransportFuture<'a, RetentionReceipts> {
            Box::pin(async { unreachable!() })
        }
        fn finished<'a>(
            &'a mut self,
            _: &'a RunnerHttp,
            _: &'a RetainedAttempt,
        ) -> TransportFuture<'a, FinishedReceipt> {
            Box::pin(async { unreachable!() })
        }
    }
    let _clock = crate::native::http::test_clock::TestClock::new();
    let peer = peer().await;
    let (_dir, http, permit) = client(&peer.origin, false);
    let mut transport = PermitTransport::new(http.clone(), Exchange);
    let worker = crate::config::WorkerConfig {
        server_url: peer.origin.clone(),
        node_id: permit.binding.node.node_id,
        credential: String::new(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    transport.preflight(&worker).unwrap();
    let f = fixture("phase_v2_renewal");
    let request = serde_json::from_value(f["request"].clone()).unwrap();
    peer.status.store(503, Ordering::SeqCst);
    assert!(matches!(
        transport
            .renew(&worker, &request, Uuid::new_v4())
            .await
            .unwrap(),
        RenewalOutcome::Unavailable
    ));
    assert!(!http.authority_lost());
    peer.status.store(403, Ordering::SeqCst);
    assert!(matches!(
        transport
            .renew(&worker, &request, Uuid::new_v4())
            .await
            .unwrap(),
        RenewalOutcome::Refused
    ));
    assert!(http.authority_lost());
}
