use super::*;
use aq_admission::startup::NativeOffer;
use aq_worker::native::preparation::DurablePhaseExecution;

struct Progress<'a, R: Read> {
    input: &'a mut BufReader<R>,
    stage: &'a mut &'static str,
    result: serde_json::Value,
}
impl<R: Read> Progress<'_, R> {
    fn assessment(&mut self, deadline: u64, root: &Path, held: &File) -> Result<()> {
        *self.stage = "phase_synthetic_assessment_pause";
        self.result["phase"] = json!("prepared_inputs_awaiting_synthetic_assessment");
        emit(&self.result)?;
        continuation_before(self.input, libc::STDIN_FILENO, deadline)?;
        current_root(root, held)
    }
    async fn execution(
        &mut self,
        pipeline: &InputPipeline,
        grant: &DurablePhaseExecution,
    ) -> Result<()> {
        pipeline.check_phase_execution(grant).await?;
        self.result["phase"] = json!("granted_phase_execution");
        self.result["execution_record_digest"] = json!(grant.history().record_digest());
        self.result["execution_deadline_ns"] = json!(grant.deadline_ns());
        self.result["execution_hard_deadline_ns"] = json!(grant.hard_deadline_ns());
        self.result["execution_checked_at_ns"] = json!(boot_ns()?);
        self.result["execution_revision"] = json!(pipeline.snapshot().await?.revision);
        Ok(())
    }
}

async fn wait_for_replay(execution: bool, valid_for_ms: u64) -> Result<()> {
    if !execution {
        let pause_ms = valid_for_ms
            .checked_add(100)
            .context("phase probe pause overflow")?;
        ensure!(pause_ms <= 5000, "bounded phase probe pause required");
        tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    }
    Ok(())
}

// Each module operates on its own protocol's concrete request, evidence and prepared-input types.
macro_rules! phase_protocol {
($module:ident,$wire:ident,$prepared:ident, $terminal:path, $propose:ident, $begin_accept:ident, $accept:ident, $record_accept:ident, $check_accept:ident, $artifacts:ident, $material:ident, $artifact:ident, $hydrate:ident, $inputs:ident, $execute:ident, $history:ident, $record_execution:ident, $execution_field:ident, $refresh:literal) => {
        mod $module {
            use super::*;
            use aq_admission::$wire::{AcceptRequest, Negotiation};
            use aq_worker::native::{
                http::$wire::{AcceptedPreparation, ProposedPreparation},
                preparation::$prepared as Prepared,
            };
            use $terminal as terminal_wire;

            struct Accepted {
                proposed: ProposedPreparation,
                request: AcceptRequest,
                preparation: Arc<AcceptedPreparation>,
                deadline: u64,
            }
            struct Phase {
                config: Config,
                held: File,
                offer: NativeOffer,
                id: Uuid,
                client: NativeClient,
                pipeline: Arc<InputPipeline>,
                accepted: Accepted,
            }
            async fn accept(
                reserved: &mut setup::Reserved,
                phase: &PhaseConfig<Negotiation>,
                progress: &mut Progress<'_, impl Read>,
            ) -> Result<Accepted> {
                let setup::Reserved {
                    run,
                    offer,
                    id,
                    deadline,
                } = reserved;
                let setup::SmokeRun {
                    config,
                    client,
                    journal,
                    held,
                    ..
                } = run;
                let (id, deadline) = (*id, *deadline);
                let stage = &mut *progress.stage;
                *stage = "phase_proposal";
                let proposed = client
                    .$propose(
                        &journal.state().identity,
                        offer,
                        &phase.negotiation,
                        deadline,
                    )
                    .await?;
                *stage = "phase_accept_intent";
                let request = journal.$begin_accept(
                    id,
                    &proposed,
                    config.capabilities.clone(),
                    phase.source_profile.clone(),
                    phase.maximum_preparation_ms,
                )?;
                *stage = "phase_accept";
                let observed = client
                    .$accept(&proposed, &request, None)
                    .await?;
                *stage = "phase_received_history";
                let first = journal.$record_accept(id, &observed)?;
                journal.$check_accept(id, &observed)?;
                wait_for_replay(phase.prepare_execution, offer.offered.clock.valid_for_ms).await?;
                *stage = "phase_replay";
                let replay = client
                    .$accept(&proposed, &request, Some(&observed))
                    .await?;
                let retained = journal.$record_accept(id, &replay)?;
                let cap = journal.$check_accept(id, &replay)?;
                current_root(&config.root, held)?;
                progress.result = json!({"phase":"accepted_preparation","request":request,"proposal":proposed.proposal(),
                    "initial_delivery":observed.delivery(),"replay_delivery":replay.delivery(),
                    "history":retained,"first_history":first,"journal_revision":journal.state().revision,
                    "journal_format":journal.state().format,"original_deadline_ns":deadline,
                    "checked_at_ns":boot_ns()?,"deadline_ns":cap});
                Ok(Accepted {
                    proposed,
                    request,
                    preparation: Arc::new(replay),
                    deadline: cap,
                })
            }
            impl Phase {
                async fn inputs(
                    &self,
                    git: GitClient,
                    progress: &mut Progress<'_, impl Read>,
                ) -> Result<Prepared> {
                    let (pipeline, id, preparation) = (&self.pipeline, self.id, &self.accepted.preparation);
                    let (stage, result) = (&mut *progress.stage, &mut progress.result);
                    *stage = "phase_artifacts";
                    let artifacts = pipeline
                        .$artifacts(id, preparation.clone())
                        .await?;
                    *stage = "phase_materials";
                    for lease in &preparation.delivery().receipt.lease.materials {
                        pipeline
                            .$material(id, preparation.clone(), lease.lease_id)
                            .await?;
                    }
                    let spec = setup::spec(&self.offer, "original typed phase spec absent")?;
                    *stage = "phase_hydrate";
                    let spec = pipeline
                        .$artifact(id, preparation.clone(), spec)
                        .await?;
                    let workspace = pipeline
                        .$hydrate(id, preparation.clone(), spec, git)
                        .await?;
                    *stage = "phase_prepare_inputs";
                    let prepared = pipeline
                        .$inputs(id, preparation.clone(), artifacts, workspace)
                        .await?;
                    current_root(&self.config.root, &self.held)?;
                    // Actual opaque owners remain alive through observation. Emitted DTOs
                    // are diagnostic history and cannot create execution authority.
                    let snapshot = pipeline.snapshot().await?;
                    result["phase"] = json!("prepared_phase_inputs");
                    result["prepare_request"] = json!(prepared.request());
                    result["workspace_path"] = json!(prepared.workspace().workspace().path);
                    result["material_receipts"] = json!(
                        prepared
                            .materials()
                            .iter()
                            .map(|m| m.receipt())
                            .collect::<Result<Vec<_>>>()?
                    );
                    result["prepared_revision"] = json!(snapshot.revision);
                    result["prepared_at_ns"] = json!(boot_ns()?);
                    Ok(prepared)
                }
                async fn history(
                    &self,
                    prepared: &Prepared,
                    progress: &mut Progress<'_, impl Read>,
                ) -> Result<()> {
                    let (stage, result) = (&mut *progress.stage, &mut progress.result);
                    let (preparation, id) = (&self.accepted.preparation, self.id);
                    *stage = "phase_execution_history_only";
                    let q = terminal_wire::HistoryRequest {
                        schema_version: 1,
                        negotiation: terminal_wire::Negotiation::current()?,
                        operation_id: Uuid::new_v4(),
                        binding: prepared.request().original.binding.clone(),
                        accepted_receipt_digest: preparation.delivery().receipt.digest()?,
                        prepare_operation_id: prepared.request().original.operation_id,
                    };
                    let receipt = self.client.$history(&q).await?;
                    let known = receipt
                        .identity
                        .as_ref()
                        .context("actual issued execution history required")?;
                    let lease = &preparation.delivery().receipt.lease;
                    ensure!(
                        known.prepare_request_digest == prepared.request().digest()?
                            && known.guardian_id == lease.guardian_id
                            && known.launch_id == lease.launch_id
                            && known.reservation_id == lease.hold.reservation.reservation_id,
                        "history differs from actual Prepared/Accepted pins"
                    );
                    current_root(&self.config.root, &self.held)?;
                    let snapshot = self.pipeline.snapshot().await?;
                    let attempt = snapshot
                        .attempts
                        .get(&id)
                        .context("retained phase attempt absent")?;
                    ensure!(
                        known.runtime == attempt.reservation.runtime.runtime
                            && known.qualification == attempt.reservation.runtime.qualification
                            && attempt.$execution_field.is_none()
                            && attempt.native_host.is_none()
                            && snapshot.revision
                                == result["prepared_revision"]
                                    .as_u64()
                                    .context("prepared revision absent")?,
                        "history must not adopt execution or mutate journal"
                    );
                    result["phase"] = json!("phase_history_only");
                    result["history_request"] = json!(q);
                    result["history_receipt"] = json!(receipt);
                    result["history_revision"] = json!(snapshot.revision);
                    Ok(())
                }

                async fn execute(
                    &self,
                    phase: PhaseConfig<Negotiation>,
                    prepared: Prepared,
                    progress: &mut Progress<'_, impl Read>,
                ) -> Result<()> {
                    if phase.await_synthetic_assessment {
                        progress.assessment(
                        self.accepted.deadline,
                        &self.config.root,
                        &self.held,
                    )?;
                    }
                    *progress.stage = "phase_execution_http";
                    let a = &self.accepted;
                    let observation = self
                        .client
                        .$execute(
                            &a.proposed,
                            &a.request,
                            &a.preparation,
                            prepared.request(),
                            None,
                        )
                        .await;
                    if phase.history_after_lost_execution {
                        expected_lost_execution_reply(observation.as_ref().err())?;
                        self.history(&prepared, progress).await?;
                        emit(&progress.result)?;
                        drop(prepared);
                        return Ok(());
                    }
                    let observation = Arc::new(observation?);
                    progress.result["execution_delivery"] = json!(observation.delivery());
                    *progress.stage = "phase_execution_durable";
                    let PhaseExecutionCommit::Usable(grant) = self
                        .pipeline
                        .$record_execution(prepared, observation.clone())
                        .await?
                    else {
                        anyhow::bail!("phase execution retained as history only");
                    };
                    progress.execution(&self.pipeline, &grant).await?;
                    if let Some(host) = phase.synthetic_host {
                        let (result, input, stage) = (
                            &mut progress.result,
                            &mut *progress.input,
                            &mut *progress.stage,
                        );
                        physical::Fixture {
                            config: &self.config,
                            offer: &self.offer,
                            held: &self.held,
                            pipeline: &self.pipeline,
                            id: self.id,
                            grant: &grant,
                            launch_id: self.accepted.preparation.delivery().receipt.lease.launch_id,
                            report: phase.report_terminal,
                            relay: phase.relay_output,
                            refresh: $refresh,
                            result,
                        }
                        .run(host, input, stage)
                        .await?;
                    }
                    emit(&progress.result)
                }
            }
            pub(super) async fn run(
                mut reserved: setup::Reserved,
                phase: PhaseConfig<Negotiation>,
                progress: &mut Progress<'_, impl Read>,
            ) -> Result<()> {
                let accepted = accept(&mut reserved, &phase, progress).await?;
                if !phase.prepare_inputs {
                    return emit(&progress.result);
                }
                let setup::Reserved { run, id, offer, .. } = reserved;
                let setup::SmokeRun {
                    config,
                    held,
                    client,
                    git,
                    journal,
                    materials,
                    ..
                } = run;
                let pipeline = Arc::new(InputPipeline::new(journal, materials, client.clone()));
                let session = Phase {
                    config,
                    held,
                    offer,
                    id,
                    client,
                    pipeline,
                    accepted,
                };
                let prepared = session.inputs(git, progress).await?;
                if phase.prepare_execution {
                    return session.execute(phase, prepared, progress).await;
                }
                // This inputs-only probe completes without granting execution.
                drop(prepared);
                emit(&progress.result)
            }
        }
    };
}
phase_protocol!(
    preparation,
    preparation_v2,
    PreparedPhaseInputs,
    aq_phase_terminal_protocol,
    propose_preparation,
    begin_phase_accept,
    accept_preparation,
    record_phase_preparation,
    check_phase_preparation,
    artifacts_phase,
    material_phase,
    artifact_phase,
    hydrate_source_phase,
    prepare_inputs_phase,
    prepare_execution,
    phase_terminal_history,
    record_phase_execution,
    phase_execution,
    false
);
phase_protocol!(
    refresh,
    preparation_refresh_v1,
    PreparedRefreshInputs,
    aq_phase_terminal_protocol::refresh_v1,
    propose_refresh_preparation,
    begin_refresh_accept,
    accept_refresh_preparation,
    record_refresh_preparation,
    check_refresh_preparation,
    artifacts_refresh,
    material_refresh,
    artifact_refresh,
    hydrate_source_refresh,
    prepare_inputs_refresh,
    prepare_refresh_execution,
    refresh_terminal_history,
    record_refresh_execution,
    refresh_execution,
    true
);
pub(super) async fn run(
    mut reserved: setup::Reserved,
    input: &mut BufReader<impl Read>,
    stage: &mut &'static str,
) -> Result<()> {
    let mut progress = Progress {
        input,
        stage,
        result: serde_json::Value::Null,
    };
    if let Some(phase) = reserved.run.config.phase_preparation.take() {
        return preparation::run(reserved, phase, &mut progress).await;
    }
    let phase = reserved
        .run
        .config
        .refresh_preparation
        .take()
        .context("phase preparation profile required")?;
    refresh::run(reserved, phase, &mut progress).await
}
