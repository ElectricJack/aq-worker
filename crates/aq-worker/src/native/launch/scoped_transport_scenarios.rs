//! Safety-fence IO checks only. No host, Grant, or stopped proof is fabricated.
use super::*;
use crate::native::adapter::responses_transport::{
    ResponsesScope, ScopedResponsesTransport, UpstreamEndpoint,
};
use aq_session_protocol::Text;
use tokio::io::AsyncReadExt;
#[tokio::test]
async fn logical_stop_fence_closes_transport_before_any_async_physical_stop_poll() {
    let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/startup.fixture.json"
    ))
    .unwrap();
    let mut offer: aq_admission::startup::NativeOffer =
        serde_json::from_value(fixture["native_offer"].clone()).unwrap();
    let m = &mut offer.offered.offer.manifest;
    m.model.provider = Text::new("openai").unwrap();
    m.model.reasoning = Text::new("medium").unwrap();
    m.model.spending_budget = None;
    offer.offered.offer.binding.launch_manifest_digest =
        crate::native::journal::canonical_digest(m).unwrap();
    let binding = offer.offered.offer.binding;
    let id = binding.attempt_id;
    let owner = ScopedResponsesTransport::start(
        ResponsesScope::new(
            binding.clone(),
            &m.model,
            boot_ns().unwrap() + 10_000_000_000,
        )
        .unwrap(),
        UpstreamEndpoint::development_loopback("http://127.0.0.1:9").unwrap(),
        zeroize::Zeroizing::new(b"local-only-capability".to_vec()),
        zeroize::Zeroizing::new(b"upstream-unused-key".to_vec()),
    )
    .await
    .unwrap();
    let address = owner.origin().strip_prefix("http://").unwrap().to_owned();
    let mut pending = tokio::net::TcpStream::connect(&address).await.unwrap();
    let state = crate::native::journal::State {
        format: crate::native::journal::FORMAT,
        identity: aq_admission::transport::WorkerIdentity {
            node_id: Uuid::new_v4(),
            boot_id: Uuid::new_v4(),
        },
        kernel_boot_id: Uuid::new_v4(),
        revision: 0,
        attempts: BTreeMap::new(),
    };
    let registry = Registry::new(&state);
    registry.admit(id).unwrap();
    let entry = Arc::new(Entry {
        id,
        scope: None,
        result_channel: None,
        profile: None,
        history: Mutex::new(None),
        stop: AtomicBool::new(false),
        hook: registry.hook.clone(),
        fault: registry.fault.clone(),
        physical: Mutex::new(Physical {
            handle: None,
            observer: None,
            pending_output: Vec::new(),
            stopped: None,
            transport: Some(owner),
            outcome: Outcome::Unknown,
        }),
    });
    registry
        .entries
        .lock()
        .unwrap()
        .entries
        .insert(id, entry.clone());
    registry.fence(id).unwrap();
    assert!(entry.stop.load(Ordering::SeqCst));
    let closed = tokio::time::timeout(Duration::from_secs(1), pending.read_to_end(&mut Vec::new()))
        .await
        .unwrap();
    assert!(
        closed.is_ok() || closed.is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset)
    );
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    assert!(entry.physical.lock().unwrap().stopped.is_none());
}
