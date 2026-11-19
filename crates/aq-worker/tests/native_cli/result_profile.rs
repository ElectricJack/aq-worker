use aq_admission::{ArtifactKind, RetainedArtifact};
use aq_session_protocol::Text;
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, Validate};
use aq_worker::native::{
    ArtifactAssembly, VerifiedArtifact,
    adapter::result_client::MAX_RESULT_CLIENT_INSTALLATION_BYTES,
    adapter::result_profile::{ExpectedResultAdapter, ResultAdapterDefinition, ResultAdapterKind},
    journal::RuntimeIdentity,
};
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
        additional_permission_digests: vec![],
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
        })
        .unwrap();
    for mode in 0..3 {
        let wrong = reference("wrong");
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
/// The published manifest, as a Codex (`claude == false`) or Claude docker-local launch.
fn manifest(claude: bool) -> LaunchManifest {
    let published: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let mut manifest: LaunchManifest =
        serde_json::from_value(published["native_offer"]["offered"]["offer"]["manifest"].clone())
            .unwrap();
    manifest.terminal.input_enabled = false;
    manifest.model.spending_budget = None;
    if !claude {
        manifest.harness.harness = Text::new("codex").unwrap();
        manifest.harness.cli_version = Text::new("0.166.0").unwrap();
        manifest.model.provider = Text::new("openai").unwrap();
        manifest.model.reasoning = Text::new("low").unwrap();
        return manifest;
    }
    manifest.harness.harness = Text::new("claude").unwrap();
    manifest.harness.cli_version = Text::new("2.1.318").unwrap();
    manifest
        .harness
        .required_capabilities
        .retain(|c| c.id.as_str() != aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    manifest
        .harness
        .required_capabilities
        .push(aq_session_protocol::Capability {
            id: Text::new(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
            version: 1,
        });
    manifest.model.provider = Text::new("anthropic").unwrap();
    // The published fixture says "standard", which the Claude recipe cannot map.
    manifest.model.reasoning = Text::new("default").unwrap();
    manifest
}
/// A role's operations as the server scopes them to one project and repository.
fn plan(
    operations: &[&str],
    project: u128,
    repository: u128,
) -> aq_session_protocol::PermissionPlan {
    let scopes: Vec<_> = operations
        .iter()
        .map(|operation| aq_session_protocol::Scope {
            operation: Text::new(*operation).unwrap(),
            resource: Text::new(format!(
                "project:{}/repository:{}",
                uuid::Uuid::from_u128(project),
                uuid::Uuid::from_u128(repository)
            ))
            .unwrap(),
        })
        .collect();
    aq_session_protocol::PermissionPlan {
        requested: scopes.clone(),
        effective: scopes.clone(),
        required: scopes,
        omissions: vec![],
    }
}
const DEVELOPER: &[&str] = &["task.read", "knowledge.read", "source.write"];
const REVIEWER: &[&str] = &["task.read", "knowledge.read"];

/// Spec 0014: the worker launches whatever permission plan a valid manifest carries. Another
/// role's operations, and the same operations on another project and repository, validate on
/// both result profiles against one installed definition; the adapter, the environment and the
/// manifest's own validity are still checked.
#[test]
fn any_permission_plan_of_a_valid_manifest_is_accepted_on_both_result_profiles() {
    for claude in [false, true] {
        let mut manifest = manifest(claude);
        let mut value = definition();
        if claude {
            value.profile = ResultAdapterKind::Claude2318DockerLocalV1;
            value.recipe = value.recipe_artifact();
        }
        value.environment = manifest.harness.environment.clone();
        // What the definition was installed with: one project's developer plan.
        value.permission_digest = permission_digest(&plan(DEVELOPER, 1, 1));
        manifest.harness.adapter = value.artifact().unwrap();
        for offered in [
            plan(DEVELOPER, 1, 1),
            plan(REVIEWER, 1, 1),
            plan(DEVELOPER, 2, 7),
            plan(REVIEWER, 2, 7),
            plan(&[], 3, 3),
        ] {
            let mut launch = manifest.clone();
            launch.permissions = offered;
            launch.validate().unwrap();
            value.validate_manifest(&launch).unwrap();
        }
        let mut invalid = manifest.clone();
        invalid.permissions = plan(REVIEWER, 1, 1);
        invalid.permissions.effective.clear();
        assert!(
            value.validate_manifest(&invalid).is_err(),
            "a plan that denies a required scope is not a valid manifest"
        );
        let mut wrong_adapter = manifest.clone();
        wrong_adapter.harness.adapter = reference("other adapter");
        assert!(value.validate_manifest(&wrong_adapter).is_err());
        let mut wrong_environment = manifest.clone();
        wrong_environment.harness.environment = reference("other environment");
        assert!(value.validate_manifest(&wrong_environment).is_err());
        let mut wrong_harness = manifest.clone();
        wrong_harness.harness.cli_version = Text::new("0.0.1").unwrap();
        assert!(value.validate_manifest(&wrong_harness).is_err());
    }
}

fn permission_digest(value: &impl serde::Serialize) -> Digest {
    let mut value = serde_json::to_value(value).unwrap();
    value.sort_all_objects();
    Digest::of(&serde_json::to_vec(&value).unwrap())
}
/// Installed definitions keep their bytes and digest: the old permission keys are still read
/// (on either profile, whatever they list) and no longer decide anything.
#[test]
fn a_definition_still_carrying_the_old_permission_keys_loads_and_is_selected() {
    for claude in [false, true] {
        let mut value = definition();
        if claude {
            value.profile = ResultAdapterKind::Claude2318DockerLocalV1;
            value.recipe = value.recipe_artifact();
        }
        let without = value.artifact().unwrap();
        value.additional_permission_digests = vec![
            permission_digest(&plan(REVIEWER, 1, 1)),
            value.permission_digest.clone(),
        ];
        let bytes = value.encode().unwrap();
        let loaded = ResultAdapterDefinition::decode(&bytes).unwrap();
        assert_eq!(loaded.encode().unwrap(), bytes);
        let adapter = loaded.artifact().unwrap();
        assert_ne!(adapter, without, "the keys stay part of the installed body");
        let runtime = RuntimeIdentity {
            runtime: loaded.runtime.clone(),
            qualification: reference("independent-runtime-qualification"),
        };
        loaded
            .validate_selected(ExpectedResultAdapter {
                adapter: &adapter,
                runtime: &runtime,
                environment: &loaded.environment,
            })
            .unwrap();
    }
}

#[test]
fn claude_input_profile_is_explicit_and_preserves_off_setting_and_old_pins() {
    let mut launch = manifest(true);
    let mut adapter = definition();
    adapter.profile = ResultAdapterKind::Claude2318DockerLocalInteractiveV1;
    adapter.recipe = adapter.recipe_artifact();
    adapter.environment = launch.harness.environment.clone();
    launch.harness.adapter = adapter.artifact().unwrap();
    assert!(adapter.interactive() && adapter.is_claude_docker());
    for enabled in [false, true] {
        launch.terminal.input_enabled = enabled;
        adapter.validate_manifest(&launch).unwrap();
    }
    let fresh_recipe = adapter.recipe.clone();
    adapter.profile = ResultAdapterKind::Claude2318DockerLocalV1;
    adapter.recipe = adapter.recipe_artifact();
    assert_ne!(adapter.recipe, fresh_recipe);
    launch.harness.adapter = adapter.artifact().unwrap();
    assert!(adapter.validate_manifest(&launch).is_err());
    launch.terminal.input_enabled = false;
    adapter.validate_manifest(&launch).unwrap();
}
