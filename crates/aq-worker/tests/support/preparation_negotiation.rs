use serde_json::Value;

pub(crate) fn negotiation(refresh: bool) -> Value {
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
