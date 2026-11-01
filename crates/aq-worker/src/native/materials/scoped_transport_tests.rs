//! Actual private file owners + finite loopback HTTP only; no admission/host claim.
use super::*;
use crate::native::adapter::responses_transport::{
    ResponsesScope, ScopedResponsesTransport, UpstreamEndpoint,
};
use aq_session_protocol::{LaunchBinding, LaunchManifest};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
fn installed() -> (
    tempfile::TempDir,
    InstalledMaterial,
    LaunchManifest,
    LaunchBinding,
) {
    let startup: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/startup.fixture.json"
    ))
    .unwrap();
    let mut offer: aq_admission::startup::NativeOffer =
        serde_json::from_value(startup["native_offer"].clone()).unwrap();
    let manifest = &mut offer.offered.offer.manifest;
    manifest.model.provider = Text::new("openai").unwrap();
    manifest.model.model = Text::new("offline-model").unwrap();
    manifest.model.reasoning = Text::new("medium").unwrap();
    manifest.model.spending_budget = None;
    offer.offered.offer.binding.launch_manifest_digest = canonical_digest(manifest).unwrap();
    let manifest = manifest.clone();
    let binding = offer.offered.offer.binding;
    let control: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/control.fixture.json"
    ))
    .unwrap();
    let prep = control["replies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "preparation")
        .unwrap();
    let mut lease: aq_admission::MaterialLease =
        serde_json::from_value(prep["body"]["original"]["materials"][0].clone()).unwrap();
    let key = b"synthetic-provider-key";
    lease.binding = binding.clone();
    lease.provider = aq_admission::Label::new("openai").unwrap();
    lease.material_digest = Digest::of(key);
    lease.material_bytes = key.len() as u64;
    let clock = aq_admission::transport::DeliveryClock {
        server_time_ms: 1500,
        expires_at_ms: 2000,
        valid_for_ms: 500,
    };
    let sent = boot_ns().unwrap();
    let received = sent + 1000;
    let worker = match binding.session {
        aq_session_protocol::SessionIdentity::Remote {
            node_id, boot_id, ..
        } => WorkerIdentity { node_id, boot_id },
        _ => unreachable!(),
    };
    let record = Record {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        preparation_digest: Digest::of(b"offline preparation only"),
        worker,
        kernel_boot_id: Uuid::new_v4(),
        clock_source: "CLOCK_BOOTTIME".into(),
        retrieval_deadline_ns: clock.client_deadline_ns(sent, received).unwrap(),
        delivery: MaterialDelivery { lease, clock },
        sent_ns: sent,
        received_ns: received,
        material_synced_ns: received + 1,
    };
    record.validate().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = File::open(root.path()).unwrap();
    write_member(&directory, c"material.bin", key).unwrap();
    write_member(
        &directory,
        c"record.json",
        &canonical_bytes(&record).unwrap(),
    )
    .unwrap();
    (
        root,
        InstalledMaterial { directory, record },
        manifest,
        binding,
    )
}
#[tokio::test]
async fn retained_transport_generates_fresh_cap_and_refuses_changed_original_before_forwarding() {
    use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
    let (root, original, manifest, binding) = installed();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let count = count.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-provider-key");
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["max_output_tokens"], 4096);
                count.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"type\":\"response.completed\"}\n\n",
                )
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let scope = || {
        ResponsesScope::new(
            binding.clone(),
            &manifest.model,
            boot_ns().unwrap() + 10_000_000_000,
        )
        .unwrap()
    };
    let first = ScopedResponsesTransport::start_retained(
        scope(),
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        original.retained_copy().unwrap(),
        &manifest,
    )
    .await
    .unwrap();
    let second = ScopedResponsesTransport::start_retained(
        scope(),
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        original.retained_copy().unwrap(),
        &manifest,
    )
    .await
    .unwrap();
    let cap = first.local_capability();
    assert_eq!(cap.len(), 77);
    assert_ne!(cap.as_slice(), b"synthetic-provider-key");
    assert_ne!(cap.as_slice(), second.local_capability().as_slice());
    second.stop().await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let endpoint = format!("{}/v1/responses", first.origin());
    let body = serde_json::json!({"model":"offline-model","stream":true,"store":false,"input":[{"type":"message","role":"user","content":"literal"}],"reasoning":{"effort":"medium"},"tools":[]});
    let send = || {
        client
            .post(&endpoint)
            .bearer_auth(std::str::from_utf8(&cap).unwrap())
            .json(&body)
            .send()
    };
    assert!(send().await.unwrap().status().is_success());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    std::fs::write(root.path().join("record.json"), b"changed original record").unwrap();
    assert!(send().await.is_err());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    std::fs::write(
        root.path().join("record.json"),
        canonical_bytes(&original.record).unwrap(),
    )
    .unwrap();
    std::fs::write(root.path().join("material.bin"), b"changed-provider-key").unwrap();
    assert!(send().await.is_err());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    first.stop().await.unwrap();
    server.abort();
}
#[tokio::test]
async fn retained_transport_refuses_manifest_substitution_without_starting_upstream() {
    let (_root, original, mut manifest, binding) = installed();
    let scope = ResponsesScope::new(
        binding,
        &manifest.model,
        boot_ns().unwrap() + 10_000_000_000,
    )
    .unwrap();
    manifest.model.model = Text::new("foreign-model").unwrap();
    assert!(
        ScopedResponsesTransport::start_retained(
            scope,
            UpstreamEndpoint::openai(),
            original,
            &manifest
        )
        .await
        .is_err()
    );
}
