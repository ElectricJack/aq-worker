use aq_session_api::{ContractDocument, decode};
use std::{fs, path::PathBuf};
fn publication() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol/session-api/v1")
}
#[test]
fn schema_is_published_without_drift() {
    let checked: serde_json::Value =
        serde_json::from_slice(&fs::read(publication().join("schema.json")).unwrap()).unwrap();
    assert_eq!(checked, aq_session_api::schema());
}
#[test]
fn published_examples_round_trip_shared_identity_and_unknown_input() {
    for path in fs::read_dir(publication().join("examples")).unwrap() {
        let bytes = fs::read(path.unwrap().path()).unwrap();
        let doc = decode(&bytes).unwrap();
        let encoded = serde_json::to_vec(&doc).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap()
        );
        if let ContractDocument::InputReceipt(r) = &doc {
            assert!(matches!(
                r.disposition,
                aq_session_api::InputDisposition::DeliveryUnknown
            ));
            assert!(r.input_paused);
        }
        decode(&encoded).unwrap();
    }
}
#[test]
fn mutations_reject_principal_and_unknown_fields_and_oversize_input() {
    let mut v: serde_json::Value = serde_json::from_slice(
        &fs::read(publication().join("examples/remote-input.json")).unwrap(),
    )
    .unwrap();
    v["body"]["principal_id"] = "chosen-by-caller".into();
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
    v["body"].as_object_mut().unwrap().remove("principal_id");
    v["body"]["bytes"] = serde_json::json!(vec![0u8; 4097]);
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
    v["body"]["bytes"] = serde_json::json!([]);
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
}
#[test]
fn requests_have_no_selected_principal_or_cross_scope_authority() {
    let schema = aq_session_api::schema();
    let defs = schema["$defs"].as_object().unwrap();
    for name in [
        "WatchRequest",
        "AcquireControlRequest",
        "InputRequest",
        "ResizeRequest",
        "MessageRequest",
        "ResolveInputRequest",
        "ProfileRequest",
    ] {
        assert_eq!(defs[name]["additionalProperties"], false);
        assert!(defs[name]["properties"].get("principal_id").is_none());
        assert!(defs[name]["properties"].get("tenant_id").is_none());
    }
}
#[test]
fn identical_operation_replay_has_same_digest_changed_payload_conflicts() {
    let original = fs::read(publication().join("examples/remote-input.json")).unwrap();
    let doc = decode(&original).unwrap();
    let first = aq_session_api::request_digest(&doc).unwrap();
    let reencoded = serde_json::to_vec(&doc).unwrap();
    assert_eq!(
        first,
        aq_session_api::request_digest(&decode(&reencoded).unwrap()).unwrap()
    );
    let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
    changed["body"]["bytes"] = serde_json::json!([98, 10]);
    let changed = decode(&serde_json::to_vec(&changed).unwrap()).unwrap();
    assert_ne!(first, aq_session_api::request_digest(&changed).unwrap());
    // The daemon must persist the first digest and reject the latter under the same ID.
}
fn fixture(name: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(publication().join("examples").join(name)).unwrap()).unwrap()
}
fn rejected(v: &serde_json::Value) {
    assert!(
        decode(&serde_json::to_vec(v).unwrap()).is_err(),
        "accepted {v}"
    );
}
#[test]
fn nil_ids_zero_revisions_and_controller_fences_are_rejected() {
    for field in ["operation_id", "lease_id"] {
        let mut v = fixture("remote-input.json");
        v["body"][field] = Uuid::nil().to_string().into();
        rejected(&v);
    }
    for field in [
        "expected_session_revision",
        "expected_controller_generation",
        "expires_at_ms",
    ] {
        let mut v = fixture("remote-input.json");
        v["body"][field] = 0.into();
        rejected(&v);
    }
    let mut v = fixture("profile-validate.json");
    v["body"]["project_binding_id"] = Uuid::nil().to_string().into();
    rejected(&v);
    let v = fixture("central-acquire_control.json");
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_ok());
}
#[test]
fn explanation_provenance_and_relations_are_checked() {
    for field in ["catalog_digest", "agent_profile", "task_context_revision"] {
        let mut v = fixture("profile-explanation.json");
        v["body"]["request"][field] = "".into();
        rejected(&v);
    }
    let base = fixture("profile-explanation.json");
    let mut v = base.clone();
    v["body"]["selection"]["selected_candidate_id"] = serde_json::Value::Null;
    rejected(&v);
    let mut v = base.clone();
    v["body"]["selection"]["candidates"][0]["eligibility"] = "policy_denied".into();
    rejected(&v);
    let mut v = base.clone();
    let c = v["body"]["selection"]["candidates"][0].clone();
    v["body"]["selection"]["candidates"]
        .as_array_mut()
        .unwrap()
        .push(c);
    rejected(&v);
    let mut v = base.clone();
    v["body"]["effective_permissions"][0]["operation"] = "source.write".into();
    rejected(&v);
    let mut v = base.clone();
    v["body"]["effective_permissions"][0]["scope_reference"] = "*".into();
    rejected(&v);
    let mut v = base.clone();
    v["body"]["agent_profile"]["digest"] = "A".repeat(64).into();
    rejected(&v);
    let mut v = base.clone();
    v["body"]["required_bytes"] = u64::MAX.into();
    rejected(&v);
    let mut v = fixture("central-input_receipt.json");
    v["body"]["input_paused"] = false.into();
    rejected(&v);
}
#[test]
fn watch_and_acquire_are_requests_not_controller_permits() {
    for name in ["central-watch.json", "central-acquire_control.json"] {
        for field in [
            "lease_id",
            "principal_id",
            "tenant_id",
            "effective_permissions",
            "bytes",
        ] {
            let mut v = fixture(name);
            v["body"][field] = "forged".into();
            rejected(&v);
        }
    }
    let mut v = fixture("remote-input.json");
    v["body"]["terminal"]["session"]["scope"] = "*".into();
    rejected(&v);
}
use uuid::Uuid;
#[test]
fn watch_coverage_and_resolution_capture_fields_are_validated() {
    for field in ["snapshot_id", "subscription_id"] {
        let mut v = fixture("central-watch_view.json");
        v["body"][field] = Uuid::nil().to_string().into();
        rejected(&v);
    }
    for field in ["last_observed_at_ms", "columns", "rows"] {
        let mut v = fixture("central-watch_view.json");
        v["body"][field] = 0.into();
        rejected(&v);
    }
    let mut v = fixture("central-watch_view.json");
    v["body"]["coverage"] = "complete".into();
    v["body"]["truncated"] = true.into();
    rejected(&v);
    for field in [
        "fresh_snapshot_id",
        "fresh_capture_request_id",
        "uncertain_operation_id",
    ] {
        let mut v = fixture("central-resolve_input.json");
        v["body"][field] = Uuid::nil().to_string().into();
        rejected(&v);
        let mut v = fixture("central-resolve_input.json");
        v["body"].as_object_mut().unwrap().remove(field);
        rejected(&v);
    }
}
#[test]
fn pinned_worker_publication_is_consumed_by_human_endpoint() {
    use aq_session_protocol::{ContractDocument as WorkerDocument, Validate};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/aq-session-protocol-0.1.0/artifacts/fixtures.json");
    let f: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    for v in f["valid_documents"].as_array().unwrap() {
        let doc: WorkerDocument =
            aq_session_protocol::decode(&serde_json::to_vec(v).unwrap()).unwrap();
        doc.validate().unwrap();
        if let WorkerDocument::Launch(offer) = doc {
            assert_eq!(
                serde_json::to_value(
                    aq_session_protocol::canonical_digest(&offer.manifest).unwrap()
                )
                .unwrap(),
                f["manifest_digest"]
            );
        }
    }
    let command: aq_session_protocol::InputCommand =
        aq_session_protocol::decode(&serde_json::to_vec(&f["input"]).unwrap()).unwrap();
    let human = serde_json::json!({"contract":"input","body":{
        "terminal":command.lease.terminal,"operation_id":command.operation_id,
        "lease_id":command.lease.lease_id,"expected_controller_generation":command.lease.generation,
        "expected_session_revision":1,"expires_at_ms":command.deadline_ms,"bytes":command.bytes
    }});
    decode(&serde_json::to_vec(&human).unwrap()).unwrap();
}
#[test]
fn pinned_graph_access_profile_is_a_distinct_namespace() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/agent-access/v1/schema.json");
    let access: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(access["$id"], "urn:agent-q:agent-access:1");
    assert!(access["$defs"]["ResolvedProfile"].is_object());
    let human = aq_session_api::schema();
    assert!(human["$defs"].get("ResolvedProfile").is_none());
    let mut v = fixture("profile-explanation.json");
    v["body"]["agent_profile"] = access["$defs"]["ResolvedProfile"].clone();
    rejected(&v);
}
#[test]
fn duplicate_known_json_fields_refuse_before_canonicalization() {
    let original = serde_json::to_string(&fixture("remote-input.json")).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let needle = format!("\"operation_id\":\"{id}\"");
    let raw = original.replacen(&needle, &format!("{needle},{needle}"), 1);
    assert!(decode(raw.as_bytes()).is_err());
    let raw = original.replacen("\"epoch\":1", "\"epoch\":1,\"epoch\":1", 1);
    assert!(decode(raw.as_bytes()).is_err());
    let message = serde_json::json!({"contract":"message","body":{
        "session":fixture("central-input.json")["body"]["terminal"]["session"],
        "operation_id":id,"expected_session_revision":1,"expires_at_ms":2000,"text":"hello"
    }})
    .to_string();
    let raw = message.replacen(
        "\"text\":\"hello\"",
        "\"text\":\"hello\",\"text\":\"changed\"",
        1,
    );
    assert!(decode(raw.as_bytes()).is_err());
}
