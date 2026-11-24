use super::*;
impl AcknowledgedInventory {
    pub fn receipt(&self) -> &wire::InventoryReceipt {
        &self.receipt
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
}
impl InventoryOperation {
    pub fn request(&self) -> &wire::InventoryV2 {
        &self.inner.request
    }
    pub(crate) fn begin_send(&self, now: u64) -> Result<(wire::InventoryV2, PresenceSendGuard)> {
        let (_, guard) = begin(&self.inner.tx, now)?;
        Ok((self.inner.request.clone(), guard))
    }
    pub(crate) fn finish(
        &self,
        receipt: wire::InventoryReceipt,
        received: u64,
    ) -> Result<AcknowledgedInventory> {
        receipt.validate_for(&self.inner.request)?;
        let first = lock(&self.inner.tx)?
            .first_sent_ns
            .context("inventory was not sent")?;
        let deadline = first
            .checked_add(
                (receipt.expires_at_ms - receipt.recorded_at_ms)
                    .checked_mul(1_000_000)
                    .context("inventory clock overflow")?,
            )
            .context("inventory clock overflow")?;
        ensure!(received >= first, "inventory receive clock regression");
        let mut saved = lock(&self.inner.receipt)?;
        if let Some(old) = saved.as_ref() {
            ensure!(same(old, &receipt)?, "inventory replay receipt changed");
        } else {
            *saved = Some(receipt.clone());
        }
        Ok(AcknowledgedInventory {
            origin: self.inner.origin.clone(),
            receipt,
            deadline_ns: deadline,
        })
    }
    pub(super) fn acknowledged(&self) -> Result<bool> {
        Ok(lock(&self.inner.receipt)?.is_some())
    }
}
impl ScopedPresenceProducer {
    pub(super) fn inventory_request(
        &self,
        id: Uuid,
        sequence: u64,
        ttl: u64,
    ) -> Result<wire::InventoryV2> {
        ensure!(
            (1000..=self.profile.body.maximum_ttl_ms).contains(&ttl),
            "inventory TTL exceeds qualification"
        );
        Ok(wire::InventoryV2 {
            schema_version: 1,
            operation_id: id,
            identity: self.identity.clone(),
            runtime: self.profile.body.runtime.clone(),
            runtime_qualification: self.profile.body.runtime_qualification.clone(),
            negotiation: wire::Negotiation {
                protocol: wire::Protocol::ScopedMaterialPresenceV1,
                qualification: self.profile.reference.clone(),
            },
            sequence,
            ttl_ms: ttl,
            qualifications: self.harnesses.clone(),
            provisionable_only: vec![],
        })
    }
    pub fn begin_inventory(&self, ttl_ms: u64) -> Result<InventoryOperation> {
        let mut state = lock(&self.state)?;
        if let Some(old) = &state.inventory {
            ensure!(
                old.acknowledged()?,
                "unresolved inventory operation retained"
            );
        }
        let sequence = self.pipeline.inventory.next(state.inventory_sequence)?;
        let request = self.inventory_request(Uuid::new_v4(), sequence, ttl_ms)?;
        request.validate()?;
        let operation = InventoryOperation {
            inner: Arc::new(InventoryInner {
                origin: self.origin.clone(),
                request,
                tx: Arc::new(Mutex::new(Tx::default())),
                receipt: Mutex::new(None),
            }),
        };
        state.inventory_sequence = sequence;
        state.inventory = Some(InventoryOperation {
            inner: operation.inner.clone(),
        });
        Ok(operation)
    }
    pub async fn publish_inventory(
        &self,
        operation: &InventoryOperation,
    ) -> Result<AcknowledgedInventory> {
        ensure!(
            Arc::ptr_eq(&self.origin, &operation.inner.origin),
            "inventory belongs to another producer"
        );
        {
            let state = lock(&self.state)?;
            ensure!(
                state
                    .inventory
                    .as_ref()
                    .is_some_and(|o| Arc::ptr_eq(&o.inner, &operation.inner)),
                "inventory operation superseded"
            );
        }
        self.pipeline
            .storage(|owner| {
                let _ = owner.journal.available()?;
                Ok(())
            })
            .await?;
        self.pipeline.client.scoped_inventory(operation).await
    }
}
