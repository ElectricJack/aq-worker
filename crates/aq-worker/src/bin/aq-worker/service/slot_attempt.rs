use super::*;
use aq_worker::native::result::StoppedSubmissionPending;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Merger(tokio::task::JoinHandle<()>);
impl Drop for Merger {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl NativeSlotBackend {
    fn mark_active(&self, node: Uuid) -> Active {
        let counter = self.native_active.get(&node).cloned().unwrap_or_default();
        counter.fetch_add(1, Ordering::SeqCst);
        Active(counter)
    }
    fn merge_pause(
        &self,
        node: Uuid,
        mut native_pause: watch::Receiver<bool>,
    ) -> (Merger, watch::Receiver<bool>) {
        let mut manual_pause = self
            .manual_pause
            .get(&node)
            .cloned()
            .unwrap_or_else(|| watch::channel(false).1);
        let (combined, pause) = watch::channel(*native_pause.borrow() || *manual_pause.borrow());
        let merge = tokio::spawn(async move {
            let mut manual_live = true;
            loop {
                combined.send_replace(*native_pause.borrow() || *manual_pause.borrow());
                tokio::select! {
                    r = native_pause.changed() => { if r.is_err() { break; } },
                    r = manual_pause.changed(), if manual_live => { manual_live = r.is_ok(); }
                }
            }
        });
        (Merger(merge), pause)
    }

    pub(super) async fn run_slot_attempt(
        &self,
        ctx: SlotContext,
        shutdown: watch::Receiver<bool>,
        pause: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome> {
        let session = self.sessions.get(&ctx.node_id).cloned();
        let _permit = if let Some(session) = &session
            && session.aware()
        {
            // Lost the race for the room `ready` saw: back to waiting there.
            let Some(permit) = session.try_queued() else {
                return Ok(AttemptOutcome::Paused);
            };
            Some(permit)
        } else {
            None
        };
        let _active = self.mark_active(ctx.node_id);
        let (_merge, pause) = self.merge_pause(ctx.node_id, pause);
        let worker = WorkerConfig::load(&ctx.enrollment)?;
        let cfg = self.native_config(&ctx)?;
        let mut executor = ClaudeDockerExecutor::configured(&worker)?.with_conversation_outbox(
            self.conversation_root
                .as_ref()
                .map(|root| root.join(ctx.node_id.to_string())),
        );
        executor.observations = self.observations.get(&ctx.node_id).cloned();
        executor.transcript_bound = self.transcript_bound;
        let result = if let Some(session) = session {
            run_node_slot(
                &worker,
                cfg,
                &mut executor,
                shutdown,
                (pause, ctx.hold),
                session,
                ctx.slot,
            )
            .await
        } else {
            run_until_attempt(&worker, cfg, &mut executor, shutdown, pause, ctx.hold).await
        };
        let pending = result
            .as_ref()
            .err()
            .and_then(|failure| failure.downcast_ref::<StoppedSubmissionPending>());
        // The execution owner dropped its journal lock. Keep the slot's aq-git
        // service up so salvage can recover the original stopped sandbox and report.
        if let Some(StoppedSubmissionPending(attempt)) = pending {
            let config = self.native_config(&ctx)?;
            if matches!(
                Box::pin(salvage_interrupted(&worker, &config, *attempt)).await?,
                SalvageDisposition::Salvaged | SalvageDisposition::AlreadyReported
            ) {
                return Ok(AttemptOutcome::AttemptStopped);
            }
        }
        result.map(slot_outcome)
    }
}
fn slot_outcome(disposition: RunDisposition) -> AttemptOutcome {
    match disposition {
        RunDisposition::AttemptStopped => AttemptOutcome::AttemptStopped,
        RunDisposition::PollingPaused => AttemptOutcome::Paused,
        RunDisposition::ShutdownBeforeAdmission | RunDisposition::NoOffer => {
            AttemptOutcome::ShutdownBeforeAdmission
        }
    }
}
