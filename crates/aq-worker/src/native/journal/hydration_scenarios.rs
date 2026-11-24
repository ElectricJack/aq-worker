fn published_phase_fixture() -> serde_json::Value {
    serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
    ))
    .unwrap()
}
fn refresh_registry_fact(
    f: &mut aq_admission::preparation_v2::OwnerFact,
    c: &aq_admission::preparation_v2::ChoicePins,
    _: &aq_admission::preparation_v2::AcceptedReceipt,
) {
    if let aq_admission::preparation_v2::FactValue::Registry {
        registry,
        credential_profile,
        credential_revision,
    } = &mut f.body.value
    {
        *registry = c.registry.clone();
        *credential_profile = c.credential_profile.clone();
        *credential_revision = c.credential_revision;
    }
}
super::physical_fixture::physical_fixture! {aq_admission::preparation_v2,aq_phase_terminal_protocol,preparation_v2,phase_acceptance,phase_preparation,propose_preparation,begin_phase_accept,accept_preparation,record_phase_preparation}
#[path = "granted_scenarios.rs"]
mod granted_scenarios;
#[path = "../../../tests/support/node_git_playbook.rs"]
mod node_git_playbook;

#[path = "presence_scenarios.rs"]
pub(super) mod presence_scenarios;

#[path = "phase_execution_scenarios.rs"]
mod phase_execution_scenarios;

#[path = "native_host_scenarios.rs"]
mod native_host_scenarios;

async fn checkout_fixture() -> (Inputs, GitPeer) {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    (h, git)
}
async fn hydrate(
    h: &Inputs,
    git: &GitClient,
) -> anyhow::Result<crate::native::preparation::HydratedWorkspace> {
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.clone())
        .await
}

include!("hydration_scenarios/replay.rs");

include!("hydration_scenarios/join.rs");

include!("hydration_scenarios/inline.rs");

include!("hydration_scenarios/phase_inputs.rs");

include!("hydration_scenarios/pinned_source.rs");

include!("hydration_scenarios/repository.rs");
