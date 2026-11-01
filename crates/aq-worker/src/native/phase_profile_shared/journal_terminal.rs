macro_rules! journal_terminal {
    ($wire:path,$acceptance:ident,$preparation:ident,$prepared:ident,$execution:ident,$terminal:ident,$ack_started:ident,$ack_stopped:ident) => {
        // Historical outbox only. Loading this module never constructs live authority.
        /// Charged from outer intent before any host effects. Existing snapshot cap stays8MiB.
        pub use super::phase_terminal_budget::{BASE_HISTORY_MAX, HOST_HISTORY_BUDGET};
        use super::*;
        use $wire as wire;

        #[derive(Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct PhaseTerminalHistory {
            pub started: StartedPublication,
            pub stopped: Option<StoppedPublication>,
        }
        #[derive(Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct StartedPublication {
            pub request: wire::StartedRequest,
            pub recorded_revision: u64,
            pub receipt: Option<wire::StartedReceipt>,
        }
        #[derive(Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct StoppedPublication {
            pub request: wire::StoppedRequest,
            pub recorded_revision: u64,
            pub receipt: Option<wire::StoppedReceipt>,
        }
        pub(crate) fn with_context<T>(
            a: &Attempt,
            f: impl FnOnce(&wire::Context<'_>) -> Result<T>,
        ) -> Result<T> {
            let execution = a
                .$execution
                .as_ref()
                .context("terminal missing actual execution")?;
            let accepted = a
                .$preparation
                .as_ref()
                .context("terminal missing Accepted")?;
            let acceptance = a
                .$acceptance
                .as_ref()
                .context("terminal missing actual phase intent")?;
            let prepare = &a
                .$prepared
                .as_ref()
                .context("terminal missing phase Prepare")?
                .request;
            let prepared = aq_admission::granted_closure::JournalCheckpoint {
                revision: prepare.original.journal_revision,
                digest: prepare.original.journal_digest.clone(),
            };
            let context = wire::Context {
                execution: &execution.delivery.receipt,
                prepare,
                accepted: &accepted.delivery.receipt,
                acceptance: &acceptance.request,
                proposal: &acceptance.proposal,
                runtime: &a.reservation.runtime.runtime,
                runtime_qualification: &a.reservation.runtime.qualification,
                prepared: &prepared,
            };
            context.validate()?;
            f(&context)
        }
        pub(crate) fn started(
            a: &Attempt,
            q: aq_admission::started::Request,
            revision: u64,
        ) -> Result<PhaseTerminalHistory> {
            with_context(a, |c| {
                let request = wire::StartedRequest {
                    schema_version: 1,
                    negotiation: wire::Negotiation::current()?,
                    origin: wire::Origin::for_context(c)?,
                    original: q,
                };
                request.validate_for(c)?;
                Ok(PhaseTerminalHistory {
                    started: StartedPublication {
                        request,
                        recorded_revision: revision,
                        receipt: None,
                    },
                    stopped: None,
                })
            })
        }
        pub(crate) fn stopped(
            a: &Attempt,
            proof: &aq_session_host::StoppedProof,
            revision: u64,
            state: &State,
        ) -> Result<Option<StoppedPublication>> {
            let h = a
                .native_host
                .as_ref()
                .context("terminal missing actual host")?;
            let Some(t) = &h.$terminal else {
                return Ok(None);
            }; // No genuine Running: keep stop/Unknown held; no invented Started/closure.
            let mapped: aq_admission::stopped::HostStoppedProof =
                serde_json::from_value(serde_json::to_value(proof)?)?;
            let i = &h.intent;
            let original = aq_admission::stopped::Request {
                schema_version: 1,
                evidence: aq_admission::transport::WorkerStopEvidence {
                    schema_version: 1,
                    binding: i.binding.clone(),
                    guardian_id: i.logical_guardian_id,
                    launch_id: i.launch_id,
                    stop_operation_id: i.stopped_operation_id,
                    journal: aq_admission::transport::JournalEvidence::Recorded {
                        revision: state.revision,
                        digest: canonical_digest(state)?,
                    },
                    disposition: aq_admission::transport::StopDisposition::Stopped {
                        grant_id: i.grant_id,
                        stop_receipt_digest: mapped.digest()?,
                    },
                    reason: aq_admission::transport::StopReason::Reconciliation,
                },
                reservation_id: i.reservation_id,
                runtime: i.runtime.runtime.clone(),
                qualification: i.runtime.qualification.clone(),
                proof: mapped,
            };
            let request = wire::StoppedRequest {
                schema_version: 1,
                negotiation: t.started.request.negotiation.clone(),
                origin: t.started.request.origin.clone(),
                original,
            };
            with_context(a, |c| {
                request.validate_for(c, &t.started.request)?;
                Ok(())
            })?;
            Ok(Some(StoppedPublication {
                request,
                recorded_revision: revision,
                receipt: None,
            }))
        }
        pub(super) fn validate(a: &Attempt, revision: u64) -> Result<()> {
            let Some(h) = &a.native_host else {
                return Ok(());
            };
            if a.$acceptance.is_none() {
                ensure!(h.$terminal.is_none(), "terminal outbox under wrong profile");
                return Ok(());
            }
            let Some(t) = &h.$terminal else {
                ensure!(h.running.is_none(), "Running lacks paired terminal outbox");
                return Ok(());
            };
            with_context(a, |c| {
                t.started.request.validate_for(c)?;
                let running = h
                    .running
                    .as_ref()
                    .context("Started outbox without genuine Running")?;
                ensure!(
                    same(&running.request, &t.started.request.original)?
                        && t.started.recorded_revision == running.recorded_revision
                        && t.started.recorded_revision <= revision,
                    "Started outbox original checkpoint"
                );
                if let Some(r) = &t.started.receipt {
                    r.validate_for(&t.started.request)?;
                }
                if let Some(s) = &t.stopped {
                    s.request.validate_for(c, &t.started.request)?;
                    let physical = h
                        .stopped
                        .as_ref()
                        .context("Stopped outbox without actual opaque stop history")?;
                    ensure!(
                        same(&s.request.original.proof.observation, &physical.observation)?
                            && s.recorded_revision == physical.recorded_revision
                            && s.recorded_revision <= revision
                            && s.request.original.journal()?.0.checked_add(1)
                                == Some(s.recorded_revision),
                        "Stopped outbox physical/checkpoint mismatch"
                    );
                    if let Some(r) = &s.receipt {
                        r.validate_for(&s.request)?;
                    }
                } else {
                    ensure!(h.stopped.is_none(), "Running stop lacks paired outbox");
                }
                Ok(())
            })?;
            ensure!(
                canonical_bytes(t)?.len() <= 4 * wire::MAX_BYTES + 1024,
                "terminal four-document bound"
            );
            Ok(())
        }

        impl Journal {
            pub(crate) fn $ack_started(
                &mut self,
                id: Uuid,
                receipt: wire::StartedReceipt,
            ) -> Result<()> {
                self.storage.check_owner()?;
                ensure!(!self.poisoned, "native writer poisoned");
                let h = self
                    .state
                    .attempts
                    .get(&id)
                    .and_then(|a| a.native_host.as_ref())
                    .and_then(|h| h.$terminal.as_ref())
                    .context("phase Started outbox absent")?;
                receipt.validate_for(&h.started.request)?;
                if let Some(old) = &h.started.receipt {
                    ensure!(same(old, &receipt)?, "changed phase Started ACK");
                    return Ok(());
                }
                let mut next = self.next()?;
                next.attempts
                    .get_mut(&id)
                    .unwrap()
                    .native_host
                    .as_mut()
                    .unwrap()
                    .$terminal
                    .as_mut()
                    .unwrap()
                    .started
                    .receipt = Some(receipt);
                self.persist(next)
            }
            pub(crate) fn $ack_stopped(
                &mut self,
                id: Uuid,
                receipt: wire::StoppedReceipt,
            ) -> Result<()> {
                self.storage.check_owner()?;
                ensure!(!self.poisoned, "native writer poisoned");
                let h = self
                    .state
                    .attempts
                    .get(&id)
                    .and_then(|a| a.native_host.as_ref())
                    .and_then(|h| h.$terminal.as_ref())
                    .and_then(|t| t.stopped.as_ref())
                    .context("phase Stopped outbox absent")?;
                receipt.validate_for(&h.request)?;
                if let Some(old) = &h.receipt {
                    ensure!(same(old, &receipt)?, "changed phase Stopped ACK");
                    return Ok(());
                }
                let mut next = self.next()?;
                next.attempts
                    .get_mut(&id)
                    .unwrap()
                    .native_host
                    .as_mut()
                    .unwrap()
                    .$terminal
                    .as_mut()
                    .unwrap()
                    .stopped
                    .as_mut()
                    .unwrap()
                    .receipt = Some(receipt);
                self.persist(next)
            }
        }
    };
}
pub(crate) use journal_terminal;
