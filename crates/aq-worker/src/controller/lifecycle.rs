use super::*;
use aq_admission_sandbox::sandbox::PermitScope;
use zeroize::Zeroizing;

impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub async fn launch(&mut self, id: Uuid, request: &P::Request) -> Result<()> {
        let mut a = self.attempt(id)?;
        self.admit_effect(&a)?;
        ensure!(
            a.phase == Phase::Accepted,
            "launch already attempted or not accepted"
        );
        a.phase = Phase::CreateIntent;
        self.save(id, a.clone())?;
        let sandbox = self.backend.create(&a.create).await?;
        ensure!(
            sandbox.kind == self.backend.kind()
                && sandbox.owner == a.create.owner
                && !sandbox.backend_id.is_empty(),
            "backend returned mismatched identity"
        );
        a.sandbox = Some(sandbox.clone());
        a.phase = Phase::Created;
        self.save(id, a.clone())?;
        self.admit_effect(&a)?;
        a.phase = Phase::IssueIntent;
        self.save(id, a.clone())?;
        let material = self
            .provider
            .issue(request, &sandbox, &a.request.manifest)
            .await?;
        let permit = &material.permit;
        permit.validate()?;
        let now = now_ms()?;
        ensure!(
            permit.scope == PermitScope::Preparation
                && permit.binding.node == *self.identity()
                && permit.binding.attempt_id == id
                && permit.binding.grant_generation
                    == a.request.manifest.bindings.assignment_generation
                && permit.hard_deadline_ms == a.request.hard_deadline_ms
                && permit.issued_at_ms <= now
                && now < permit.expires_at_ms,
            "delegated permit does not authorize this preparation"
        );
        ensure!(
            !self
                .state()
                .attempts
                .values()
                .any(|other| other.permit_id == Some(permit.permit_id)
                    || other.logical_sandbox_id == Some(permit.binding.sandbox_id)
                    || other.channel_id == Some(permit.binding.channel_id)),
            "delegated permit reused"
        );
        // Credentials and manifest are only in private files; the snapshot holds IDs.
        // Failures here poison this invocation, preventing uncertain handoff retries.
        let permit_bytes = Zeroizing::new(serde_json::to_vec(permit)?);
        let manifest_bytes = Zeroizing::new(serde_json::to_vec(&a.request.manifest)?);
        self.journal.poisoned = true;
        self.journal
            .storage
            .write_private(&format!("permit-{id}.json"), &permit_bytes)?;
        self.journal
            .storage
            .write_private(&format!("manifest-{id}.json"), &manifest_bytes)?;
        self.journal.poisoned = false;
        a.permit_id = Some(permit.permit_id);
        a.logical_sandbox_id = Some(permit.binding.sandbox_id);
        a.channel_id = Some(permit.binding.channel_id);
        a.phase = Phase::MaterialIntent;
        self.save(id, a.clone())?;
        self.backend
            .deliver_private_material(&sandbox, material.payload)
            .await?;
        a.phase = Phase::MaterialDelivered;
        self.save(id, a.clone())?;
        self.admit_effect(&a)?;
        ensure!(
            now_ms()? < permit.expires_at_ms,
            "delegated permit expired before start"
        );
        a.phase = Phase::StartIntent;
        self.save(id, a.clone())?;
        self.backend.start_runner(&sandbox).await?;
        a.phase = Phase::Running;
        self.save(id, a)
    }
    fn record_proof(&mut self, id: Uuid, mut a: Attempt, proof: &StoppedWriterProof) -> Result<()> {
        ensure!(
            a.sandbox.as_ref() == Some(proof.sandbox()),
            "backend proof names another sandbox"
        );
        if proof.kind() == WriterProofKind::Destroyed
            && !(matches!(a.phase, Phase::DestroyIntent | Phase::Destroyed)
                && a.destroy_authorization
                    .as_ref()
                    .is_some_and(|auth| auth.authorizes(proof.sandbox())))
        {
            self.journal.poisoned = true;
            anyhow::bail!("backend reported destruction without durable destroy authority");
        }
        a.phase = match proof.kind() {
            WriterProofKind::StoppedNoProcesses => Phase::Stopped,
            WriterProofKind::Destroyed => Phase::Destroyed,
        };
        a.stopped = Some(proof.clone());
        self.save(id, a)
    }
    pub async fn inspect(&mut self, id: Uuid) -> Result<InventoryEntry> {
        let a = self.attempt(id)?;
        let sandbox = a
            .sandbox
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox identity not recorded"))?;
        let inventory = self.backend.inspect(sandbox).await?;
        ensure!(
            inventory.sandbox == *sandbox
                && inventory.name == a.create.name
                && inventory.template_digest == a.create.template_digest
                && inventory.hard_deadline_ms == a.create.hard_deadline_ms,
            "backend inventory mismatch"
        );
        if a.stopped.is_some() && !inventory.writer_proof().is_confirmed() {
            self.journal.poisoned = true;
            anyhow::bail!("backend contradicted durable stopped-writer proof");
        }
        if let Confirmation::Confirmed(proof) = inventory.writer_proof() {
            self.record_proof(id, a, &proof)?;
        }
        Ok(inventory)
    }
    pub async fn stop(&mut self, id: Uuid) -> Result<Confirmation<StoppedWriterProof>> {
        let mut a = self.attempt(id)?;
        if let Some(proof) = a.stopped {
            return Ok(Confirmation::Confirmed(proof));
        }
        let sandbox = a.sandbox.clone().ok_or_else(|| {
            anyhow::anyhow!("sandbox identity not recorded; reconciliation required")
        })?;
        a.phase = Phase::StopIntent;
        self.save(id, a.clone())?;
        let confirmation = self.backend.stop(&sandbox).await;
        if let Confirmation::Confirmed(proof) = &confirmation {
            self.record_proof(id, a, proof)?;
        }
        Ok(confirmation)
    }
    /// The caller supplies authenticated retention/operator authority; this
    /// method never manufactures permission to delete sandbox evidence.
    pub async fn destroy(
        &mut self,
        id: Uuid,
        authorization: &DestroyAuthorization,
    ) -> Result<Confirmation<StoppedWriterProof>> {
        let mut a = self.attempt(id)?;
        let sandbox = a
            .sandbox
            .clone()
            .ok_or_else(|| anyhow::anyhow!("sandbox identity not recorded"))?;
        ensure!(
            a.stopped.is_some() && authorization.authorizes(&sandbox),
            "destroy requires stopped writer and retention authority"
        );
        if a.phase == Phase::Destroyed {
            return Ok(Confirmation::Confirmed(a.stopped.unwrap()));
        }
        a.destroy_authorization = Some(authorization.clone());
        a.phase = Phase::DestroyIntent;
        self.save(id, a.clone())?;
        let confirmation = self.backend.destroy(&sandbox, authorization).await;
        if let Confirmation::Confirmed(proof) = &confirmation {
            ensure!(
                proof.kind() == WriterProofKind::Destroyed,
                "destroy not confirmed"
            );
            self.record_proof(id, a, proof)?;
        }
        Ok(confirmation)
    }
}
