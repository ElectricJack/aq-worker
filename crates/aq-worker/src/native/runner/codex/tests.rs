use super::*;
use aq_session_protocol::{Capability, Text};
use std::os::unix::fs::PermissionsExt;
fn worker() -> WorkerConfig {
    WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::new_v4(),
        credential: "fixture-node-credential".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
}
fn settings(config: &NativeConfig) -> CodexSettings {
    let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let manifest = &fixture["native_offer"]["offered"]["offer"]["manifest"];
    let mut harness: HarnessSettings = serde_json::from_value(manifest["harness"].clone()).unwrap();
    harness.harness = Text::new("codex").unwrap();
    harness.cli_version = Text::new("0.166.0").unwrap();
    harness.adapter = config.adapter.clone();
    harness.required_capabilities.clear();
    let mut model: ModelSettings = serde_json::from_value(manifest["model"].clone()).unwrap();
    model.provider = Text::new("openai").unwrap();
    model.reasoning = Text::new("low").unwrap();
    model.spending_budget = None;
    let root = config.state_dir.join("host");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    CodexSettings {
        host_root: root,
        harness,
        model,
        permissions_digest: Digest::of(b"permissions"),
        columns: 80,
        rows: 24,
    }
}
#[test]
fn production_factory_refuses_plaintext_and_proxy_credentials_before_admission() {
    let mut worker = worker();
    assert!(CodexExecutor::new(&worker).is_err());
    worker.server_url = "https://example.invalid".into();
    worker.cf_client_id = Some("fixture-id".into());
    worker.cf_client_secret = Some("fixture-secret".into());
    assert!(CodexExecutor::new(&worker).is_err());
}
#[cfg(feature = "test-support")]
#[test]
fn concrete_preflight_refuses_missing_settings_and_legacy_installation() {
    let (_dir, mut config) = super::super::tests::config();
    let mut executor = CodexExecutor::development_loopback(&worker()).unwrap();
    assert!(executor.preflight(&config).is_err());
    config.executor = Some(ExecutorConfig::Codex {
        settings: settings(&config),
    });
    assert!(
        executor.preflight(&config).is_err(),
        "old generic manifest cannot activate native execution"
    );
    assert!(executor.profile.is_none());
    assert!(!config.state_dir.join("journal").exists());
}
#[test]
fn closed_settings_refuse_unsupported_advertised_functions_and_mutable_handoff_kind() {
    let (_dir, mut config) = super::super::tests::config();
    let settings = settings(&config);
    settings.validate(&config).unwrap();
    config.capabilities.push(Capability {
        id: Text::new("aq.harness.native-message").unwrap(),
        version: 1,
    });
    assert!(settings.validate(&config).is_err());
    assert!(
        serde_json::from_value::<ExecutorConfig>(
            serde_json::json!({"kind":"shell","command":"arbitrary"})
        )
        .is_err()
    );
}

#[test]
fn production_factory_refuses_unqualified_output_binding_even_with_https() {
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let error = CodexExecutor::new(&worker)
        .err()
        .expect("production output bound must remain refused");
    assert!(error.to_string().contains("output bound is not qualified"));
    #[cfg(feature = "test-support")]
    assert!(
        CodexExecutor::development_loopback(&worker).is_err(),
        "test transport cannot target a remote origin"
    );
}
