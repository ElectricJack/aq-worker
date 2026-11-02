use super::*;
use aq_session_protocol::Text;
fn reference(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn manifest() -> ResultClientManifest {
    ResultClientManifest {
        schema_version: 1,
        profile: ResultClientKind::NamedReadonlyLocalV1,
        contract: aq_agent_result_local::artifact(),
        environment: reference("environment"),
        executable: FilePin {
            path: "/opt/aq/aq-agent".into(),
            artifact: reference("client"),
        },
        executable_bytes: 1024,
    }
}
fn validate(value: &ResultClientManifest, selected: &ResultClientManifest) -> Result<()> {
    // Recompute the proposed outer digest so a changed inner reference cannot be
    // refused only because it accidentally retained an unrelated stale digest.
    value.validate_selection(
        &ExpectedResultClient {
            manifest_digest: &value.digest()?,
            contract: &selected.contract,
            environment: &selected.environment,
            executable: &selected.executable.artifact,
        },
        &selected.environment,
    )
}
#[test]
fn result_client_selection_requires_actual_contract_and_independent_references() {
    let selected = manifest();
    validate(&selected, &selected).unwrap();
    for mode in 0..5 {
        let mut changed = selected.clone();
        match mode {
            0 => changed.schema_version = 2,
            1 => changed.contract.digest = Digest::of(b"wrong contract"),
            2 => changed.environment.digest = Digest::of(b"wrong environment"),
            3 => changed.executable.artifact.digest = Digest::of(b"wrong client"),
            _ => changed.executable_bytes = MAX_RESULT_CLIENT_INSTALLATION_BYTES + 1,
        }
        assert!(validate(&changed, &selected).is_err(), "mode {mode}");
    }
    let mut unsupported = selected.clone();
    unsupported.contract = reference("some-other-contract");
    assert!(
        validate(&unsupported, &unsupported).is_err(),
        "matching caller assertions cannot replace the frozen public contract"
    );
}
#[test]
fn result_client_exact_digest_environment_observation_and_byte_bounds_are_separate() {
    let selected = manifest();
    let wrong_digest = Digest::of(b"wrong installation");
    let digest = selected.digest().unwrap();
    fn expected<'a>(
        outer: &'a Digest,
        selected: &'a ResultClientManifest,
    ) -> ExpectedResultClient<'a> {
        ExpectedResultClient {
            manifest_digest: outer,
            contract: &selected.contract,
            environment: &selected.environment,
            executable: &selected.executable.artifact,
        }
    }
    assert!(
        selected
            .validate_selection(&expected(&wrong_digest, &selected), &selected.environment)
            .is_err()
    );
    assert!(
        selected
            .validate_selection(
                &expected(&digest, &selected),
                &reference("different actual environment")
            )
            .is_err()
    );
    for bytes in [0, 63, MAX_RESULT_CLIENT_INSTALLATION_BYTES + 1, u64::MAX] {
        let mut changed = selected.clone();
        changed.executable_bytes = bytes;
        assert!(validate(&changed, &selected).is_err());
    }
    for bytes in [64, MAX_RESULT_CLIENT_INSTALLATION_BYTES] {
        let mut changed = selected.clone();
        changed.executable_bytes = bytes;
        validate(&changed, &selected).unwrap();
    }
}
#[test]
fn result_client_manifest_is_closed_distinct_and_bounded() {
    let value = serde_json::to_value(manifest()).unwrap();
    ResultClientManifest::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    for mode in 0..3 {
        let mut changed = value.clone();
        match mode {
            0 => changed["profile"] = serde_json::json!("codex-0.166-named-readonly-exec-stdin-v1"),
            1 => changed["environment_qualified"] = serde_json::json!(true),
            _ => {
                changed.as_object_mut().unwrap().remove("contract");
            }
        }
        assert!(ResultClientManifest::decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    }
    assert!(ResultClientManifest::decode(&[]).is_err());
    assert!(ResultClientManifest::decode(&vec![b' '; MAX_MANIFEST + 1]).is_err());
    // This is a separate explicit reservation, not a Codex image-slot alias.
    assert_eq!(MAX_RESULT_CLIENT_INSTALLATION_BYTES, 64 * 1024 * 1024);
}
