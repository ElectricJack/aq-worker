//! Exact refresh bridge over real retained refresh-profile owner records.
//! This context correlates history; it does not grant or reconstruct authority.
terminal_profile! {
    phase: aq_admission::preparation_refresh_v1,
    schema_urn: "urn:aq:phase-terminal:registry-refresh:1",
    publication: "aq-phase-terminal-protocol/0.1.1",
    admission: "0.1.10",
    contract_id: "aq.phase-terminal.registry-refresh-v1.contract",
    domain: "aq.phase-terminal-refresh",
    feature_count: 3,
    extra_features: [RegistryObservationContinuityV1],
}
fn profile_manifest(value: &mut serde_json::Value) -> Result<()> {
    value["preparation_profile"] = serde_json::to_value(
        aq_admission::preparation_refresh_v1::contract_artifact()
            .map_err(|_| Invalid("refresh admission profile"))?,
    )
    .map_err(|_| Invalid("refresh admission encoding"))?;
    Ok(())
}
