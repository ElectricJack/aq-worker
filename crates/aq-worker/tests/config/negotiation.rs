use aq_worker::native::runner::ConfiguredPreparation;
use serde_json::json;

#[path = "../support/preparation_negotiation.rs"]
mod fixture;
use fixture::negotiation;

#[test]
fn full_artifact_features_and_unknown_fields_are_closed_without_fallback() {
    for refresh in [false, true] {
        for mode in 0..8 {
            let mut raw = negotiation(refresh);
            match mode {
                0 => raw["contract"]["id"] = json!("wrong-owner"),
                1 => raw["contract"]["version"] = json!("future"),
                2 => {
                    raw["contract"]["digest"] =
                        json!(aq_session_protocol::Digest::of(b"wrong artifact"))
                }
                3 => raw["features"]
                    .as_array_mut()
                    .unwrap()
                    .pop()
                    .map(|_| ())
                    .unwrap(),
                4 => {
                    let duplicate = raw["features"][0].clone();
                    raw["features"].as_array_mut().unwrap().push(duplicate)
                }
                5 => raw["features"][0] = json!("unknown_future_feature"),
                6 => raw["extra"] = json!(true),
                _ => raw["contract"]["extra"] = json!(true),
            }
            assert!(
                serde_json::from_value::<ConfiguredPreparation>(raw).is_err(),
                "refresh={refresh} mode={mode}"
            );
        }
        let mut crossed = negotiation(refresh);
        crossed["features"] = negotiation(!refresh)["features"].clone();
        assert!(serde_json::from_value::<ConfiguredPreparation>(crossed).is_err());
    }
}
#[test]
fn duplicate_fields_are_rejected_before_any_value_materialization() {
    for refresh in [false, true] {
        let raw = negotiation(refresh);
        let contract = serde_json::to_string(&raw["contract"]).unwrap();
        let features = serde_json::to_string(&raw["features"]).unwrap();
        for bytes in [
            format!("{{\"contract\":{contract},\"contract\":{contract},\"features\":{features}}}"),
            format!("{{\"contract\":{contract},\"features\":{features},\"features\":{features}}}"),
        ] {
            assert!(serde_json::from_str::<ConfiguredPreparation>(&bytes).is_err());
        }
        let id = serde_json::to_string(&raw["contract"]["id"]).unwrap();
        let version = serde_json::to_string(&raw["contract"]["version"]).unwrap();
        let digest = serde_json::to_string(&raw["contract"]["digest"]).unwrap();
        let bytes = format!(
            "{{\"contract\":{{\"id\":{id},\"id\":{id},\"version\":{version},\"digest\":{digest}}},\"features\":{features}}}"
        );
        assert!(serde_json::from_str::<ConfiguredPreparation>(&bytes).is_err());
    }
}
