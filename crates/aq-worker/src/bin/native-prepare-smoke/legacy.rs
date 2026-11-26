use super::*;
pub(super) async fn run(reserved: setup::Reserved, stage: &mut &'static str) -> Result<()> {
    let setup::Reserved {
        run,
        offer,
        deadline,
        id,
    } = reserved;
    let setup::SmokeRun {
        config,
        held,
        git,
        client,
        mut journal,
        materials,
        ..
    } = run;
    let accept = journal.begin_accept(id, config.capabilities)?;
    *stage = "accept";
    let preparation = Arc::new(client.accept(&accept, deadline, None).await?);
    journal.record_preparation(id, &preparation)?;
    let pipeline = InputPipeline::new(journal, materials, client.clone());
    *stage = "artifacts";
    let artifacts = pipeline.artifacts(id, preparation.clone()).await?;
    *stage = "materials";
    for lease in &preparation.plan().materials {
        pipeline
            .material(id, preparation.clone(), lease.lease_id)
            .await?;
    }
    // hydrate_source consumes its verified spec; download the exact same planned
    // requests again rather than fabricating/cloning a proof from its receipt.
    let spec = setup::spec(&offer, "original typed spec absent")?;
    *stage = "hydrate";
    let spec = pipeline.artifact(id, preparation.clone(), spec).await?;
    let workspace = pipeline
        .hydrate_source(id, preparation.clone(), spec, git)
        .await?;
    *stage = "prepare_inputs";
    let prepared = pipeline
        .prepare_inputs(id, preparation.clone(), artifacts, workspace)
        .await?;
    current_root(&config.root, &held)?;
    *stage = "initial_grant";
    let grant = client
        .prepare(prepared.request(), &preparation, &offer, None)
        .await?;
    let receipts = prepared
        .materials()
        .iter()
        .map(|m| m.receipt())
        .collect::<Result<Vec<_>>>()?;
    // Capture bounded diagnostic DTOs before transferring the actual exclusive
    // input owners. These serialized values never confer live authority.
    let mut result = json!({"phase":"granted","request":prepared.request(),"grant":grant.initial(),"workspace_path":prepared.workspace().workspace().path,"material_receipts":receipts});
    *stage = "durable_initial_grant";
    let committed = pipeline.record_initial_grant(prepared, grant).await?;
    let durable = match committed {
        GrantCommit::Usable(durable) => durable,
        GrantCommit::HistoryOnly(_) => {
            anyhow::bail!("received Grant retained as history only");
        }
    };
    *stage = "check_durable_initial_grant";
    pipeline.check_initial_grant(&durable).await?;
    current_root(&config.root, &held)?;
    result["durable_grant"] = grant_diagnostic(&durable);
    emit(&result)?;
    Ok(())
}

fn grant_diagnostic(
    durable: &aq_worker::native::preparation::DurableInitialGrant,
) -> serde_json::Value {
    json!({
        "attempt_id":durable.history().attempt_id(),
        "record_digest":durable.history().record_digest(),
        "observed_after_sync_ns":durable.history().observed_after_sync_ns(),
        "deadline_ns":durable.deadline_ns(),
        "hard_deadline_ns":durable.hard_deadline_ns()
    })
}
