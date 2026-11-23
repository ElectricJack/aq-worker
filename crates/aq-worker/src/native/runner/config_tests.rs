use super::*;

#[path = "../../../tests/support/preparation_negotiation.rs"]
mod fixture;
use fixture::negotiation;

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
