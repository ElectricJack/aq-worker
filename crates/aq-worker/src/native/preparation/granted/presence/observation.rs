use super::*;
impl ScopedPresenceProducer {
    pub async fn observe(&self, operation: &PresenceOperation) -> Result<()> {
        self.current(operation)?;
        {
            let mut stage = lock(&operation.inner.stage)?;
            match &*stage {
                Stage::Ready(_) => return Ok(()),
                Stage::Allocated => *stage = Stage::Reading,
                _ => anyhow::bail!("presence read already running or failed"),
            }
        }
        let op = operation.inner.clone();
        let result = self
            .pipeline
            .storage(move |owner| {
                let result = op.observe_current(owner);
                if result.is_err() {
                    *lock(&op.stage)? = Stage::Failed;
                }
                result
            })
            .await;
        if result.is_err() {
            *lock(&operation.inner.stage)? = Stage::Failed;
        }
        result
    }
}
impl ReportInner {
    fn observe_current(&self, owner: &Owned) -> Result<()> {
        let acquired = scope(owner, &self.scope, true)?;
        ensure!(
            acquired < self.inventory.deadline_ns,
            "presence inventory expired"
        );
        let (original, lease, receipt) = self.installation(owner)?;
        let grant = &original.delivery.original;
        self.issued_at_ms
            .store(grant.clock.server_time_ms, Ordering::SeqCst);
        self.hard_ms
            .store(original.delivery.execution_deadline_ms, Ordering::SeqCst);
        let identity = owner.journal.state().identity.clone();
        let (binding, grant_id, guardian_id, launch_id, reservation_id) = (
            grant.grant.binding.clone(),
            grant.grant.grant_id,
            grant.guardian_id,
            grant.launch_id,
            grant.reservation_id,
        );
        let authority = wire::AuthorityAnchor {
            kind: wire::AuthorityKind::InitialGrant,
            operation_id: original.delivery.operation_id,
            grant_revision: original.delivery.versions.grant_revision,
            expires_at_ms: grant.grant.expires_at_ms,
        };
        let (presence, observation) = self.read_presence(owner, &receipt, acquired)?;
        let report = wire::PresenceReport {
            schema_version: 1,
            operation_id: self.operation,
            identity,
            runtime: self.profile.body.runtime.clone(),
            runtime_qualification: self.profile.body.runtime_qualification.clone(),
            negotiation: wire::Negotiation {
                protocol: wire::Protocol::ScopedMaterialPresenceV1,
                qualification: self.profile.reference.clone(),
            },
            binding,
            grant_id,
            guardian_id,
            launch_id,
            reservation_id,
            lease_id: lease.lease_id,
            installation_receipt_digest: digest(&receipt)?,
            private_binding: receipt.private_binding.clone(),
            inventory: self.inventory.receipt.inventory.clone(),
            authority,
            sequence: self.sequence,
            ttl_ms: self.ttl,
            observation,
            presence,
        };
        report.validate_against(&lease, &receipt)?;
        *lock(&self.stage)? = Stage::Ready(Box::new(report));
        Ok(())
    }
    fn installation<'a>(
        &self,
        owner: &'a Owned,
    ) -> Result<(
        &'a crate::native::journal::ReceivedInitialGrant,
        aq_admission::MaterialLease,
        MaterialReceipt,
    )> {
        let a = owner
            .journal
            .state()
            .attempts
            .get(&self.scope.id)
            .context("presence attempt absent")?;
        let original = a
            .initial_grant
            .as_ref()
            .context("presence initial receipt absent")?;
        let grant = &original.delivery.original;
        ensure!(
            grant.materials.len() == 1
                && a.prepared
                    .as_ref()
                    .is_some_and(|p| p.request.materials.len() == 1),
            "presence supports one original material only"
        );
        ensure!(
            original.runtime.runtime == self.profile.body.runtime
                && original.runtime.qualification == self.profile.body.runtime_qualification,
            "presence runtime mismatch"
        );
        let lease = grant.materials[0].clone();
        let receipt = a.prepared.as_ref().unwrap().request.materials[0].clone();
        receipt.validate_against(&lease, receipt.received_at_ms)?;
        Ok((original, lease, receipt))
    }
    fn read_presence(
        &self,
        owner: &Owned,
        receipt: &MaterialReceipt,
        acquired: u64,
    ) -> Result<(wire::Presence, wire::Observation)> {
        let started = boot_ns()?;
        ensure!(
            started < self.scope.deadline.min(self.scope.hard),
            "presence permit expired before read"
        );
        let presence = owner.materials.observe_scoped(receipt);
        let completed = boot_ns()?;
        scope(owner, &self.scope, presence.is_present())?;
        ensure!(
            completed >= started && completed - started <= wire::MAX_READ_MS * 1_000_000,
            "presence read interval exceeded"
        );
        Ok((
            presence,
            wire::Observation {
                kernel_boot_id: owner.journal.state().kernel_boot_id,
                clock_source: wire::ClockSource::Boottime,
                permit_acquired_ns: acquired,
                permit_deadline_ns: self.scope.deadline.min(self.scope.hard),
                read_started_ns: started,
                read_completed_ns: completed,
                sent_ns: completed,
            },
        ))
    }
}
