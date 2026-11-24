//! Controller fixtures shared by public admission tests and private recovery scenarios.
use super::{
    CapacityLimits, Controller, FakeBackend, LaunchRequest, NodeConfig, PreserveConfig,
    RecoveryConfig, RefusalReason, UnavailableProvider,
};
use aq_admission_sandbox::{
    sandbox::{HarnessVersion, Money, TemplateAdvertisement},
    transport::WorkerIdentity,
};
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, SessionIdentity, Text};
use uuid::Uuid;
fn manifest() -> LaunchManifest {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../aq-session-protocol/artifacts/fixtures.json"
    ))
    .unwrap();
    let docs = fixture["valid_documents"].as_array().unwrap();
    serde_json::from_value(
        docs.iter().find(|d| d["kind"] == "launch").unwrap()["body"]["manifest"].clone(),
    )
    .unwrap()
}
pub(crate) fn config() -> NodeConfig {
    let m = manifest();
    NodeConfig {
        recovery: RecoveryConfig::default(),
        preserve: PreserveConfig::default(),
        runtime: ArtifactRef {
            id: Text::new("runtime").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"runtime"),
        },
        capabilities: vec![],
        features: vec![],
        templates: vec![TemplateAdvertisement {
            digest: m.harness.environment.digest.clone(),
            harnesses: vec![HarnessVersion {
                harness: m.harness.harness.as_str().to_owned(),
                version: m.harness.cli_version.as_str().to_owned(),
            }],
            estimated_cold_start_ms: 1,
            estimated_cold_start_cost: Money {
                currency: "USD".into(),
                micros: 10,
            },
        }],
        max_concurrent_sandboxes: 1,
        spend_ceiling: None,
        capacity: CapacityLimits {
            memory_bytes: 1 << 40,
            cpu_millis: 100_000,
            disk_bytes: 1 << 30,
            max_preparing: 4,
        },
    }
}
pub(crate) fn request(identity: &WorkerIdentity, id: u128) -> LaunchRequest {
    let mut m = manifest();
    m.bindings.attempt_id = Uuid::from_u128(id);
    m.resources.memory_bytes = 64 * 1024 * 1024;
    m.bindings.session = SessionIdentity::Remote {
        node_id: identity.node_id,
        boot_id: identity.boot_id,
        session_id: Uuid::from_u128(id + 100),
        attempt_id: m.bindings.attempt_id,
        assignment_generation: m.bindings.assignment_generation,
    };
    LaunchRequest {
        manifest: m,
        hard_deadline_ms: now_ms() + 60_000,
        disk_bytes: 1,
        required_capabilities: vec![],
    }
}
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
pub(crate) fn reason(error: anyhow::Error) -> RefusalReason {
    *error
        .downcast_ref::<RefusalReason>()
        .expect("typed refusal")
}
pub(crate) fn unavailable(
    root: &std::path::Path,
) -> anyhow::Result<Controller<FakeBackend, UnavailableProvider>> {
    Controller::open(
        root,
        Uuid::from_u128(1),
        config(),
        FakeBackend::default(),
        UnavailableProvider,
    )
}
