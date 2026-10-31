use aq_agent_api::*;
use serde_json::{Value, json};

fn base() -> Value {
    serde_json::from_str(include_str!("../artifacts/fixtures/base.json")).unwrap()
}

#[test]
fn published_wire_vectors_keep_their_original_dispositions() {
    for bytes in [
        include_bytes!("../artifacts/fixtures/messages.json").as_slice(),
        include_bytes!("../artifacts/fixtures/messaging.json").as_slice(),
    ] {
        let cases = parse(bytes).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                valid(case["type"].as_str().unwrap(), &case["value"]),
                case["valid"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
    let cases = parse(include_bytes!(
        "../artifacts/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    for case in cases["wire"].as_array().unwrap() {
        assert!(valid(case["type"].as_str().unwrap(), &case["value"]));
    }
}

#[test]
fn canonical_fixture_digests_and_scope_binding_are_preserved() {
    let value = base()["context"].clone();
    let binding: Binding = serde_json::from_value(value["binding"].clone()).unwrap();
    assert_eq!(digest(&value["profile"]).unwrap(), binding.profile_digest);
    assert_eq!(digest(&value["context"]).unwrap(), binding.context_digest);
    let context = Context::new(value.clone(), &binding).unwrap();
    assert_eq!(context.value(), &value);
    for field in [
        "task_id",
        "attempt_id",
        "session_id",
        "node_id",
        "boot_id",
        "project_id",
        "workspace_id",
    ] {
        let mut changed = value.clone();
        changed["binding"][field] = json!("00000000-0000-0000-0000-000000000999");
        assert!(Context::new(changed, &binding).is_err(), "{field}");
    }
    let mut changed = value.clone();
    changed["binding"]["generation"] = json!(2);
    assert!(Context::new(changed, &binding).is_err());
    for pointer in ["/profile/role", "/context/observed_at"] {
        let mut changed = value.clone();
        *changed.pointer_mut(pointer).unwrap() = json!("changed");
        assert!(Context::new(changed, &binding).is_err());
    }
}

#[test]
fn duplicate_keys_fractions_and_unknown_schema_never_validate_by_accident() {
    for bad in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"a":{"b":1,"b":2}}"#,
        br#"[1.0]"#,
        br#"1 2"#,
    ] {
        assert!(parse(bad).is_err());
    }
    assert!(parse(&vec![b' '; MAX_DOCUMENT_BYTES + 1]).is_err());
    assert!(canonical(&json!({"fraction":1.5})).is_err());
    assert!(!valid("NotAPublishedType", &json!({})));
    assert_eq!(
        canonical(&json!({"z":{"b":2,"a":"é"},"a":1})).unwrap(),
        "{\"a\":1,\"z\":{\"a\":\"é\",\"b\":2}}".as_bytes()
    );
}

#[test]
fn public_artifacts_match_inventory() {
    use sha2::{Digest, Sha256};
    let manifest: Value =
        serde_json::from_str(include_str!("../artifacts/inventory.json")).unwrap();
    for (name, entry) in manifest["files"].as_object().unwrap() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("artifacts")
            .join(name);
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(entry["bytes"], bytes.len());
        assert_eq!(entry["sha256"], format!("{:x}", Sha256::digest(bytes)));
    }
}
