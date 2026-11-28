use super::*;

struct Recovery<'a> {
    backend: &'a NativeSlotBackend,
    ctx: SlotContext,
    worker: WorkerConfig,
    cfg: NativeConfig,
    link: ServerLink,
}

impl NativeSlotBackend {
    pub(super) async fn recover_attempts(&self, ctx: SlotContext) -> Result<RecoverOutcome> {
        let worker = WorkerConfig::load(&ctx.enrollment)?;
        let cfg = self.native_config(&ctx)?;
        let link = ServerLink::from_enrollment(&EnrollmentFile::load(&ctx.enrollment)?)?;
        let recovery = Recovery {
            backend: self,
            ctx,
            worker,
            cfg,
            link,
        };
        let attempts = retained_attempts(&recovery.cfg, recovery.worker.node_id)?;
        let mut outcome = RecoverOutcome::Clean;
        for attempt in attempts {
            recovery.notify(attempt).await;
            match recovery.one(attempt).await? {
                held @ RecoverOutcome::Held(_) => return Ok(held),
                RecoverOutcome::Clean => {}
                RecoverOutcome::Reported if outcome != RecoverOutcome::Clean => {}
                next => outcome = next,
            }
        }
        Ok(outcome)
    }
}
impl Recovery<'_> {
    async fn notify(&self, attempt: Uuid) {
        let notice = HandoffNotice {
            schema_version: 1,
            attempt_id: attempt,
            reason: restart_reason(&self.ctx, attempt),
        };
        if !self.backend.notified.lock().unwrap().contains(&attempt) {
            match self.link.handoff(&notice).await {
                Ok(()) => drop(self.backend.notified.lock().unwrap().insert(attempt)),
                Err(e) => tracing::warn!(%attempt, "handoff notice: {e:#}"),
            }
        }
    }

    async fn one(&self, attempt: Uuid) -> Result<RecoverOutcome> {
        // The worker lists no repositories: the one this attempt's offer named is registered
        // again from the offer, as it was when the attempt began.
        register_recorded_source(&self.worker, &self.cfg, attempt)
            .await
            .with_context(|| format!("register the repository of attempt {attempt}"))?;
        // R4: boxed (as are the recovery futures inside): about 78 KiB inline.
        let salvaged = Box::pin(salvage_interrupted(&self.worker, &self.cfg, attempt)).await;
        if let Some((proof, why)) = unproven(&salvaged) {
            return self.unproven_attempt(attempt, proof, why).await;
        }
        disposition(attempt, salvaged)
    }
    async fn unproven_attempt(
        &self,
        attempt: Uuid,
        proof: Option<aq_protocol::StopProof>,
        why: String,
    ) -> Result<RecoverOutcome> {
        let Some(proof) = proof else {
            tracing::error!(%attempt, "recovery held: {why}");
            return Ok(RecoverOutcome::Held(why));
        };
        let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
            &self.cfg.executor
        else {
            return Ok(RecoverOutcome::Held(why));
        };
        let docker = settings.host.docker_local.clone();
        let stop = async || {
            let mut backend =
                aq_worker::backend::docker_local::DockerLocalBackend::connect_unix(docker).await?;
            stop_attempt_sandboxes(&mut backend, self.worker.node_id, attempt).await
        };
        let post = async |r| self.link.lost_attempt(&r).await;
        let claimed = claimed_attempts(&self.cfg, self.worker.node_id)?.contains(&attempt);

        Ok(release_lost(
            stop,
            post,
            &self.ctx.state_dir,
            LostAttempt {
                attempt,
                proof,
                claim_unreported: claimed,
                why: &why,
            },
        )
        .await)
    }
}

fn disposition(attempt: Uuid, salvaged: Result<SalvageDisposition>) -> Result<RecoverOutcome> {
    // The server released this attempt (its task was cancelled as the agent finished):
    // its result can never be recorded. Leave it for the archive instead of retrying
    // forever, which kept the slot from taking any other work.
    let released = |e: &anyhow::Error| {
        (e.downcast_ref::<aq_worker::native::http::HttpStatus>())
            .is_some_and(|s| s.0 == reqwest::StatusCode::GONE)
    };
    if salvaged.as_ref().err().is_some_and(released) {
        tracing::warn!(%attempt, "the server released this attempt; its unreported result is archived, not retried");
        return Ok(RecoverOutcome::Clean);
    }
    // The server granted this execution and the worker closed it before any launch
    // intent (for example a refused manifest). No host, sandbox or result exists, so
    // nothing can still write for it and holding the slot protects nothing: one such
    // refusal used to take the whole worker out for good. It is archived with the
    // slot's state; a launch that recorded an intent is never released this way.
    if never_launched(&salvaged) {
        tracing::error!(%attempt, "granted execution was never launched (no launch intent, host or result recorded): its state is archived and the slot released. The worker cannot report this closure yet, so the server keeps the attempt and its task until an operator settles it");
        return Ok(RecoverOutcome::Clean);
    }
    Ok(match salvaged? {
        SalvageDisposition::Unsalvageable(why) => RecoverOutcome::Held(why.to_owned()),
        SalvageDisposition::Salvaged => RecoverOutcome::Salvaged,
        SalvageDisposition::AlreadyReported | SalvageDisposition::NotStarted => {
            RecoverOutcome::Reported
        }
    })
}
