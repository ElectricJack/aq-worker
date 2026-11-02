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
fn https_factory_construction_is_not_preflight_or_admission() {
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let executor = CodexExecutor::new(&worker).unwrap();
    assert!(executor.profile.is_none());
    #[cfg(feature = "test-support")]
    assert!(CodexExecutor::development_loopback(&worker).is_err());
}

#[test]
fn legacy_profile_refuses_before_manifest_or_state_access() {
    let (dir, mut config) = super::super::tests::config();
    config.executor = Some(ExecutorConfig::Codex {
        settings: settings(&config),
    });
    std::fs::remove_file(&config.adapter_manifest).unwrap();
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let mut executor = CodexExecutor::new(&worker).unwrap();
    let error = executor.preflight(&config).unwrap_err();
    assert!(error.to_string().contains("legacy sealed-memfd"));
    assert!(executor.profile.is_none());
    assert!(!dir.path().join("journal").exists());
}

#[test]
fn trusted_profile_requires_independent_matching_environment_before_installation() {
    let (dir, mut config) = super::super::tests::config();
    let settings = settings(&config);
    let mut environment = settings.harness.environment.clone();
    environment.digest = Digest::of(b"different-environment");
    config.executor = Some(ExecutorConfig::TrustedNodeNamedReadonly {
        settings,
        environment,
    });
    std::fs::remove_file(&config.adapter_manifest).unwrap();
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let mut executor = CodexExecutor::new(&worker).unwrap();
    assert!(
        executor
            .preflight(&config)
            .unwrap_err()
            .to_string()
            .contains("environment differs")
    );
    assert!(executor.profile.is_none());
    assert!(!dir.path().join("journal").exists());
}

#[test]
fn model_bound_is_per_response_and_never_an_aggregate_spending_budget() {
    let (_dir, config) = super::super::tests::config();
    let mut settings = settings(&config);
    for cap in [0, 15, 1_048_577, u64::MAX] {
        settings.model.max_output_tokens = cap;
        assert!(settings.validate(&config).is_err());
    }
    for cap in [16, 4096, 1_048_576] {
        settings.model.max_output_tokens = cap;
        settings.validate(&config).unwrap();
    }
    settings.model.spending_budget = Some(aq_session_protocol::SpendingBudget {
        basis: aq_session_protocol::BudgetBasis::TaskCumulativeEnforcedCost,
        currency: Text::new("USD").unwrap(),
        limit_microunits: 1_000_000,
    });
    assert!(settings.validate(&config).is_err());
}

#[test]
fn trusted_profile_wire_is_closed_and_requires_environment_reference() {
    let (_dir, config) = super::super::tests::config();
    let settings = settings(&config);
    let mut wire = serde_json::json!({
        "kind":"codex-0.166-trusted-node-named-readonly-v1",
        "settings": {
            "host_root":settings.host_root,"harness":settings.harness,"model":settings.model,
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

#[cfg(feature = "test-support")]
#[test]
fn development_provider_is_explicit_numeric_loopback_and_preflight_only() {
    for origin in [
        "https://example.invalid",
        "http://localhost:1234",
        "http://192.0.2.1:1234",
    ] {
        assert!(
            CodexExecutor::development_loopback(&worker())
                .unwrap()
                .with_development_provider(origin)
                .is_err()
        );
    }
    let executor = CodexExecutor::development_loopback(&worker())
        .unwrap()
        .with_development_provider("http://127.0.0.1:1234")
        .unwrap();
    assert!(executor.profile.is_none());
}

fn result_configuration(
    config: &mut NativeConfig,
    mut selected: CodexSettings,
    wrong_runtime: bool,
    wrong_client_digest: bool,
) {
    use crate::native::adapter::{
        FilePin,
        result_client::{ResultClientKind, ResultClientManifest},
        result_profile::{ResultAdapterDefinition, ResultAdapterKind},
    };
    let client = ResultClientManifest {
        schema_version: 1,
        profile: ResultClientKind::NamedReadonlyLocalV1,
        contract: aq_agent_result_local::artifact(),
        environment: selected.harness.environment.clone(),
        executable: FilePin {
            path: "/opt/aq/aq-agent".into(),
            artifact: ArtifactRef {
                id: Text::new("qualified-client").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"offline manifest only"),
            },
        },
        executable_bytes: 1024,
    };
    let mut definition = ResultAdapterDefinition {
        schema_version: 1,
        profile: ResultAdapterKind::Codex0166TrustedNodeNamedReadonlyLocalV1,
        runtime: config.runtime.runtime.clone(),
        renderer: config.renderer.clone(),
        environment: selected.harness.environment.clone(),
        client_manifest: client.artifact().unwrap(),
        client_executable: client.executable.artifact.clone(),
        contract: aq_agent_result_local::artifact(),
        recipe: aq_codex_mapper::result_local_recipe_artifact(),
        permission_digest: selected.permissions_digest.clone(),
        reserved_client_bytes:
            crate::native::adapter::result_client::MAX_RESULT_CLIENT_INSTALLATION_BYTES,
    };
    if wrong_runtime {
        definition.runtime.digest = Digest::of(b"other runtime");
    }
    config.adapter = definition.artifact().unwrap();
    selected.harness.adapter = config.adapter.clone();
    let path = config.state_dir.join("result-adapter.json");
    std::fs::write(&path, definition.encode().unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    config.executor = Some(ExecutorConfig::TrustedNodeNamedReadonlyResultLocal {
        environment: selected.harness.environment.clone(),
        settings: selected,
        // Deliberately absent. Both negative selections must refuse before this
        // file or any kernel image owner is opened, and before any admission.
        client_manifest: config.state_dir.join("absent-client-manifest.json"),
        client_manifest_digest: if wrong_client_digest {
            Digest::of(b"other client installation")
        } else {
            client.digest().unwrap()
        },
        composite_definition: path,
    });
}
#[test]
fn result_factory_refuses_nonselected_client_before_kernel_or_admission() {
    let (root, mut config) = super::super::tests::config();
    let selected = settings(&config);
    result_configuration(&mut config, selected, false, true);
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let mut executor = CodexExecutor::new(&worker).unwrap();
    let error = executor.preflight(&config).unwrap_err();
    assert!(
        error.to_string().contains("client installation differs"),
        "{error:#}"
    );
    assert!(executor.profile.is_none() && executor.result_root.is_none());
    assert_eq!(executor.installation_reserved_bytes(), None);
    assert!(!root.path().join("journal").exists());
}
#[test]
fn result_factory_refuses_nonselected_runtime_even_with_matching_adapter_hash() {
    let (root, mut config) = super::super::tests::config();
    let selected = settings(&config);
    result_configuration(&mut config, selected, true, false);
    let mut worker = worker();
    worker.server_url = "https://example.invalid".into();
    let mut executor = CodexExecutor::new(&worker).unwrap();
    let error = executor.preflight(&config).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("independently selected runtime/settings"),
        "{error:#}"
    );
    assert!(executor.profile.is_none() && executor.result_root.is_none());
    assert!(!root.path().join("journal").exists());
}
#[test]
fn result_factory_wire_is_distinct_and_cannot_supply_arbitrary_folder() {
    let (_root, config) = super::super::tests::config();
    let selected = settings(&config);
    let mut wire = serde_json::json!({
        "kind":"codex-0.166-trusted-node-named-readonly-result-local-v1",
        "settings": {"host_root":selected.host_root,"harness":selected.harness,"model":selected.model,
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
