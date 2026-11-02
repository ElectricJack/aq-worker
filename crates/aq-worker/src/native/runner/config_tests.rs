use super::*;
use serde_json::{Value, json};
fn negotiation(refresh: bool) -> Value {
    if refresh {
        serde_json::to_value(aq_admission::preparation_refresh_v1::Negotiation {
            contract: aq_admission::preparation_refresh_v1::contract_artifact().unwrap(),
            features: aq_admission::preparation_refresh_v1::required_features(),
        })
        .unwrap()
    } else {
        serde_json::to_value(aq_admission::preparation_v2::Negotiation {
            contract: aq_admission::preparation_v2::contract_artifact().unwrap(),
            features: aq_admission::preparation_v2::required_features(),
        })
        .unwrap()
    }
}
#[test]
fn exact_old_wire_stays_frozen_and_new_wire_selects_refresh_explicitly() {
    for refresh in [false, true] {
        let original = negotiation(refresh);
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed: ConfiguredPreparation = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(matches!(parsed, ConfiguredPreparation::Refresh(_)), refresh);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
        let nominal_bytes = if refresh {
            serde_json::to_vec(
                &serde_json::from_value::<aq_admission::preparation_refresh_v1::Negotiation>(
                    original.clone(),
                )
                .unwrap(),
            )
            .unwrap()
        } else {
            serde_json::to_vec(
                &serde_json::from_value::<aq_admission::preparation_v2::Negotiation>(
                    original.clone(),
                )
                .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(serde_json::to_vec(&parsed).unwrap(), nominal_bytes);
        let (_dir, mut config) = super::tests::config();
        config.negotiation = parsed;
        config.validate().unwrap();
        assert_eq!(
            config.negotiation.profile() == profile::TerminalProfile::Refresh,
            refresh
        );
    }
}
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
