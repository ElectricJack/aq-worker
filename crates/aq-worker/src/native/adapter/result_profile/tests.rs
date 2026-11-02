use super::*;
use crate::native::ArtifactAssembly;
use aq_session_protocol::Text;
fn reference(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn definition() -> ResultAdapterDefinition {
    ResultAdapterDefinition {
        schema_version: 1,
        profile: ResultAdapterKind::Codex0166TrustedNodeNamedReadonlyLocalV1,
        runtime: reference("runtime"),
        environment: reference("environment"),
        client_manifest: ArtifactRef {
            id: Text::new("aq.agent-result-client-installation").unwrap(),
            ..reference("client-installation")
        },
        client_executable: reference("client-image"),
        contract: aq_agent_result_local::artifact(),
        recipe: aq_codex_mapper::result_local_recipe_artifact(),
        permission_digest: Digest::of(b"permissions"),
        reserved_client_bytes: MAX_RESULT_CLIENT_INSTALLATION_BYTES,
    }
}
fn descriptor(value: &ResultAdapterDefinition) -> RetainedArtifact {
    RetainedArtifact {
        artifact: value.artifact().unwrap(),
        kind: ArtifactKind::Definition,
        bytes: value.encode().unwrap().len() as u64,
        retention_receipt: reference("retention"),
        retained_at_ms: 1,
        retain_until_ms: 10000,
    }
}
fn retained(descriptor: RetainedArtifact, bytes: &[u8]) -> VerifiedArtifact {
    let mut download = ArtifactAssembly::new(descriptor).unwrap();
    download.append(0, bytes).unwrap();
    download.finish().unwrap()
}
#[test]
fn composite_selection_is_canonical_closed_and_independently_pinned() {
    let value = definition();
    let bytes = value.encode().unwrap();
    ResultAdapterDefinition::decode(&bytes).unwrap();
    assert!(ResultAdapterDefinition::decode(&serde_json::to_vec_pretty(&value).unwrap()).is_err());
    let runtime = RuntimeIdentity {
        runtime: value.runtime.clone(),
        qualification: reference("independent-runtime-qualification"),
    };
    let adapter = value.artifact().unwrap();
    value
        .validate_selected(ExpectedResultAdapter {
            adapter: &adapter,
            runtime: &runtime,
            environment: &value.environment,
            permission_digest: &value.permission_digest,
        })
        .unwrap();
    for mode in 0..4 {
        let wrong = reference("wrong");
        let wrong_digest = Digest::of(b"wrong");
        let runtime = RuntimeIdentity {
            runtime: if mode == 1 {
                wrong.clone()
            } else {
                value.runtime.clone()
            },
            qualification: runtime.qualification.clone(),
        };
        assert!(
            value
                .validate_selected(ExpectedResultAdapter {
                    adapter: if mode == 0 { &wrong } else { &adapter },
                    runtime: &runtime,
                    environment: if mode == 2 {
                        &wrong
                    } else {
                        &value.environment
                    },
                    permission_digest: if mode == 3 {
                        &wrong_digest
                    } else {
                        &value.permission_digest
                    },
                })
                .is_err()
        );
    }
    for mode in 0..4 {
        let mut changed = value.clone();
        match mode {
            0 => changed.contract.digest = Digest::of(b"wrong contract"),
            1 => changed.recipe.digest = Digest::of(b"wrong recipe"),
            2 => changed.reserved_client_bytes = 0,
            _ => changed.client_manifest.id = Text::new("wrong-role").unwrap(),
        }
        assert!(ResultAdapterDefinition::decode(&changed.encode().unwrap()).is_err());
    }
}

#[test]
fn selected_definition_revision_preserves_schema_and_exact_retained_pins() {
    let mut value = definition();
    let historical = value.artifact().unwrap();
    assert_eq!(historical.version.as_str(), "1");
    let revision = Text::new("2").unwrap();
    let selected = value.artifact_at_revision(&revision).unwrap();
    assert_eq!(selected.id, historical.id);
    assert_eq!(selected.digest, historical.digest);
    assert_eq!(value.artifact().unwrap(), historical);
    value.client_manifest.version = revision.clone();
    let selected = value.artifact_at_revision(&revision).unwrap();
    let runtime = RuntimeIdentity {
        runtime: value.runtime.clone(),
        qualification: reference("independent-runtime-qualification"),
    };
    value
        .validate_selected(ExpectedResultAdapter {
            adapter: &selected,
            runtime: &runtime,
            environment: &value.environment,
            permission_digest: &value.permission_digest,
        })
        .unwrap();
    let mut descriptor = descriptor(&value);
    descriptor.artifact = selected.clone();
    let body = retained(descriptor.clone(), &value.encode().unwrap());
    value
        .validate_retained(&selected, std::slice::from_ref(&descriptor), &body)
        .unwrap();
    assert!(
        value
            .validate_retained(&historical, &[descriptor], &body)
            .is_err()
    );
    let mut tampered = selected;
    tampered.digest = Digest::of(b"changed body");
    assert!(
        value
            .validate_selected(ExpectedResultAdapter {
                adapter: &tampered,
                runtime: &runtime,
                environment: &value.environment,
                permission_digest: &value.permission_digest,
            })
            .is_err()
    );
    assert_eq!(value.schema_version, 1);
    assert_eq!(value.contract, aq_agent_result_local::artifact());
    assert_eq!(
        value.recipe,
        aq_codex_mapper::result_local_recipe_artifact()
    );
}
#[test]
fn retained_definition_requires_its_original_role_descriptor_bytes_and_selection() {
    let value = definition();
    let bytes = value.encode().unwrap();
    let descriptor = descriptor(&value);
    let body = retained(descriptor.clone(), &bytes);
    let selected = value.artifact().unwrap();
    value
        .validate_retained(&selected, std::slice::from_ref(&descriptor), &body)
        .unwrap();
    assert!(
        value
            .validate_retained(
                &reference("foreign adapter"),
                std::slice::from_ref(&descriptor),
                &body
            )
            .is_err()
    );
    assert!(value.validate_retained(&selected, &[], &body).is_err());
    assert!(
        value
            .validate_retained(&selected, &[descriptor.clone(), descriptor.clone()], &body)
            .is_err()
    );
    let mut other_role = descriptor.clone();
    other_role.kind = ArtifactKind::Context;
    let context = retained(other_role.clone(), &bytes);
    assert!(
        value
            .validate_retained(&selected, &[other_role], &context)
            .is_err()
    );
    assert!(
        value
            .validate_retained(&selected, std::slice::from_ref(&descriptor), &context)
            .is_err(),
        "same bytes do not substitute a differently retained role"
    );
    let mut other_descriptor = descriptor.clone();
    other_descriptor.retain_until_ms += 1;
    assert!(
        value
            .validate_retained(&selected, &[other_descriptor], &body)
            .is_err()
    );
}
#[test]
fn result_profile_preserves_existing_permission_domain_and_pins_whole_plan() {
    let published: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let mut manifest: LaunchManifest =
        serde_json::from_value(published["native_offer"]["offered"]["offer"]["manifest"].clone())
            .unwrap();
    manifest.harness.harness = Text::new("codex").unwrap();
    manifest.harness.cli_version = Text::new("0.166.0").unwrap();
    manifest.terminal.input_enabled = false;
    manifest.model.provider = Text::new("openai").unwrap();
    manifest.model.reasoning = Text::new("low").unwrap();
    manifest.model.spending_budget = None;
    let mut value = definition();
    value.environment = manifest.harness.environment.clone();
    value.permission_digest =
        crate::native::journal::canonical_digest(&manifest.permissions).unwrap();
    manifest.harness.adapter = value.artifact().unwrap();
    value.validate_manifest(&manifest).unwrap();
    let original_adapter = manifest.harness.adapter.clone();
    manifest.harness.adapter = value
        .artifact_at_revision(&Text::new("2").unwrap())
        .unwrap();
    value.validate_manifest(&manifest).unwrap();
    manifest.harness.adapter = original_adapter;
    let mut changed = manifest.clone();
    changed
        .permissions
        .omissions
        .push(Text::new("unavailable optional scope").unwrap());
    changed.validate().unwrap();
    assert!(
        value.validate_manifest(&changed).is_err(),
        "whole compiled permission plan is pinned"
    );
    let mut selected_changed = value.clone();
    selected_changed.permission_digest =
        crate::native::journal::canonical_digest(&changed.permissions).unwrap();
    changed.harness.adapter = selected_changed.artifact().unwrap();
    selected_changed.validate_manifest(&changed).unwrap();
    let mut wrong_adapter = manifest;
    wrong_adapter.harness.adapter = reference("other adapter");
    assert!(value.validate_manifest(&wrong_adapter).is_err());
}
