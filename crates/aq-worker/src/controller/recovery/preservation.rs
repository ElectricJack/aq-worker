use super::*;
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// SBX22: first failure records the preservation; each failure doubles
    /// the bounded handoff backoff. The disk reservation is kept throughout.
    pub(super) fn preserve_failed_handoff(&mut self, id: Uuid, now: u64) -> Result<()> {
        let mut a = self.attempt(id)?;
        let interval = self.config.recovery.sweep_interval_ms;
        let p = a.preserved.get_or_insert_with(|| Preservation {
            preserved_at_ms: now,
            expires_at_ms: now.saturating_add(self.config.preserve.max_age_ms),
            bytes_held: a.request.disk_bytes,
            handoff_failures: 0,
            next_handoff_ms: now,
            notified_ms: None,
            final_notice_ms: None,
            standing_release: None,
        });
        p.handoff_failures = p.handoff_failures.saturating_add(1);
        let backoff = interval
            .saturating_mul(1u64 << p.handoff_failures.min(20))
            .min(MAX_HANDOFF_BACKOFF_MS);
        p.next_handoff_ms = now.saturating_add(backoff);
        if p.handoff_failures == 1 {
            tracing::warn!(attempt=%id, bytes_held=p.bytes_held, "sandbox preserved; handoff failed");
        }
        self.save(id, a)
    }
    async fn emit_preserved(&mut self, id: Uuid, notice: PreserveNotice) -> bool {
        let Ok(a) = self.attempt(id) else {
            return false;
        };
        let Some(p) = a.preserved else {
            return false;
        };
        let event = NodeEvent::SandboxPreserved {
            attempt_id: id,
            node: self.identity().node_id,
            bytes_held: p.bytes_held,
            expires_at: p.expires_at_ms,
            notice,
        };
        matches!(
            tokio::time::timeout(self.sweep_interval(), self.provider.emit(&event),).await,
            Ok(Ok(()))
        )
    }
    /// Notifications and `max_age` handling for one preserved, unretained sandbox.
    pub(super) async fn tend_preserved(&mut self, id: Uuid, now: u64) -> Result<()> {
        let p = self.attempt(id)?.preserved.unwrap();
        let action = self.config.preserve.expiry_action;
        if let Some(notice) = p.notice_due(action, now)
            && self.emit_preserved(id, notice).await
        {
            let mut a = self.attempt(id)?;
            let p = a.preserved.as_mut().unwrap();
            match notice {
                PreserveNotice::FinalNotice => p.final_notice_ms = Some(now),
                _ => p.notified_ms = Some(now),
            }
            self.save(id, a)?;
        }
        self.expire_preserved(id, action, now).await
    }
    async fn expire_preserved(
        &mut self,
        id: Uuid,
        action: PreserveExpiryAction,
        now: u64,
    ) -> Result<()> {
        let mut a = self.attempt(id)?;
        let p = a.preserved.clone().unwrap();
        if action != PreserveExpiryAction::Destroy {
            return Ok(());
        }
        // Destroy only after the final notice was delivered at least 24h ago.
        let Some(final_notice) = p.final_notice_ms else {
            return Ok(());
        };
        if now < p.expires_at_ms.max(final_notice.saturating_add(DAY_MS)) {
            return Ok(());
        }
        let sandbox = a.sandbox.clone().unwrap();
        let release = format!(
            "standing-release:preserve.expiry_action=destroy:attempt={id}:expires_at={}",
            p.expires_at_ms
        );
        let auth = DestroyAuthorization::operator_released(sandbox, release.clone())?;
        a.preserved.as_mut().unwrap().standing_release = Some(release);
        a.destroy_authorization = Some(auth.clone());
        // Journal the standing operator release before the destructive effect.
        self.save(id, a)?;
        tracing::warn!(attempt=%id, "preserve.max_age reached; destroying under standing release");
        self.destroy_at(id, &auth).await?;
        Ok(())
    }
}
impl Preservation {
    fn notice_due(&self, action: PreserveExpiryAction, now: u64) -> Option<PreserveNotice> {
        match (self.notified_ms, action) {
            (None, _) => Some(PreserveNotice::Preserved),
            (Some(last), PreserveExpiryAction::Notify)
                if now >= self.expires_at_ms && now >= last.saturating_add(DAY_MS) =>
            {
                Some(PreserveNotice::Reminder)
            }
            (Some(_), PreserveExpiryAction::Destroy)
                if self.final_notice_ms.is_none()
                    && now >= self.expires_at_ms.saturating_sub(DAY_MS) =>
            {
                Some(PreserveNotice::FinalNotice)
            }
            _ => None,
        }
    }
}
