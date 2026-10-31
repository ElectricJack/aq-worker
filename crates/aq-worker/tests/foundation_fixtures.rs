// Immutable foundation compatibility fixtures retained during extraction.
use aq_protocol::{
    ApiError, CompatibilityError, EnrollRequest, EnrollResponse, HeartbeatRequest,
    HeartbeatResponse, ProbeRequest, ProbeResponse,
};
use aq_worker::config::WorkerConfig;
use serde_json::Value;

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../protocol/foundation-v1.fixtures.json"
    ))
    .unwrap()
}

#[test]
fn retained_wire_fixtures_parse_against_foundation_types() {
    let f = fixtures();
    let w = f["wire"].as_array().unwrap();
    serde_json::from_value::<EnrollRequest>(w[0]["request"].clone()).unwrap();
    serde_json::from_value::<EnrollResponse>(w[0]["response"].clone()).unwrap();
    serde_json::from_value::<HeartbeatRequest>(w[1]["request"].clone()).unwrap();
    serde_json::from_value::<HeartbeatResponse>(w[1]["response"].clone()).unwrap();
    serde_json::from_value::<ProbeRequest>(w[2]["request"].clone()).unwrap();
    serde_json::from_value::<ProbeResponse>(w[2]["response"].clone()).unwrap();
    let revoked: ApiError = serde_json::from_value(w[3]["response"].clone()).unwrap();
    assert_eq!(revoked.error, aq_protocol::CREDENTIAL_REVOKED);
    let incompatible: CompatibilityError =
        serde_json::from_value(w[4]["response"].clone()).unwrap();
    assert_eq!(
        (incompatible.supported_min, incompatible.supported_max),
        aq_protocol::SUPPORTED_WORKER_PROTOCOLS
    );
}

#[test]
fn retained_worker_identity_survives_save_reload() {
    let f = fixtures();
    let cfg: WorkerConfig = toml::from_str(f["identity"]["worker_toml"].as_str().unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.toml");
    cfg.save(&path).unwrap();
    let reloaded = WorkerConfig::load(&path).unwrap();
    assert_eq!(cfg.node_id, reloaded.node_id);
    assert_eq!(cfg.server_url, reloaded.server_url);
    assert_eq!(cfg.credential, reloaded.credential);
}
