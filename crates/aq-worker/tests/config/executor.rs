use aq_session_protocol::Digest;
use aq_worker::native::runner::{CodexSettings, ExecutorConfig, NativeConfig};
#[path = "../support/codex_executor_settings.rs"]
mod executor_settings;
/// The executor settings name no model: the offered one is launched. A template that still
/// carries the old installed `model` is refused as an unknown field.
#[test]
fn executor_settings_carry_no_installed_model() {
    let (_dir, config) = super::native::config();
    let settings = executor_settings::settings(&config);
    let mut wire = serde_json::json!({
        "kind":"codex-0.166-exec-stdin-v1",
        "settings": {
            "host_root":settings.host_root,"harness":settings.harness,
            "permissions_digest":settings.permissions_digest,"columns":80,"rows":24
        }
    });
    serde_json::from_value::<ExecutorConfig>(wire.clone()).unwrap();
    wire["settings"]["model"] = serde_json::json!({"provider":"openai","model":"installed-model",
        "reasoning":"low","max_output_tokens":4096,"spending_budget":null});
    let refused = serde_json::from_value::<ExecutorConfig>(wire)
        .err()
        .unwrap();
    assert!(
        refused.to_string().contains("unknown field `model`"),
        "{refused}"
    );
}

#[test]
fn trusted_profile_wire_is_closed_and_requires_environment_reference() {
    let (_dir, config) = super::native::config();
    let settings = executor_settings::settings(&config);
    let mut wire = serde_json::json!({
        "kind":"codex-0.166-trusted-node-named-readonly-v1",
        "settings": {
            "host_root":settings.host_root,"harness":settings.harness,
            "permissions_digest":settings.permissions_digest,"columns":80,"rows":24
        },
        "environment":settings.harness.environment
    });
    assert!(matches!(
        serde_json::from_value::<ExecutorConfig>(wire.clone()).unwrap(),
        ExecutorConfig::TrustedNodeNamedReadonly { .. }
    ));
    wire["environment_qualified"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ExecutorConfig>(wire.clone()).is_err());
    wire.as_object_mut()
        .unwrap()
        .remove("environment_qualified");
    wire.as_object_mut().unwrap().remove("environment");
    assert!(serde_json::from_value::<ExecutorConfig>(wire).is_err());
}

#[test]
fn result_factory_wire_is_distinct_and_cannot_supply_arbitrary_folder() {
    let (_dir, config) = super::native::config();
    let selected = executor_settings::settings(&config);
    let mut wire = serde_json::json!({
        "kind":"codex-0.166-trusted-node-named-readonly-result-local-v1",
        "settings": {"host_root":selected.host_root,"harness":selected.harness,
            "permissions_digest":selected.permissions_digest,"columns":80,"rows":24},
        "environment":selected.harness.environment,
        "client_manifest":"/var/lib/aq/client-manifest.json",
        "client_manifest_digest":Digest::of(b"client descriptor"),
        "composite_definition":"/var/lib/aq/result-definition.json"
    });
    assert!(matches!(
        serde_json::from_value::<ExecutorConfig>(wire.clone()).unwrap(),
        ExecutorConfig::TrustedNodeNamedReadonlyResultLocal { .. }
    ));
    wire["result_root"] = serde_json::json!("/arbitrary/external/folder");
    assert!(serde_json::from_value::<ExecutorConfig>(wire.clone()).is_err());
    wire.as_object_mut().unwrap().remove("result_root");
    wire.as_object_mut().unwrap().remove("composite_definition");
    assert!(serde_json::from_value::<ExecutorConfig>(wire).is_err());
}
