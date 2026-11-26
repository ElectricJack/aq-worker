use super::*;

impl Admission {
    pub(super) async fn dispatch<E: NativeExecutor>(
        self,
        executor: &mut E,
        pause: Option<Pause>,
    ) -> Result<RunDisposition> {
        let mut shutdown = self.shutdown.clone();
        let (first, interval) = aq_node_service::polling::staggered(IDLE_POLL_INTERVAL);
        let Some((mut pause, hold)) = pause else {
            return match self.poll_once(true).await? {
                Some(found) => self.execute(executor, found).await,
                None if *shutdown.borrow() => Ok(RunDisposition::ShutdownBeforeAdmission),
                None => Ok(RunDisposition::NoOffer),
            };
        };
        tokio::time::sleep(first).await;
        let outcome = poll_until_offer(
            |offers| self.poll_once(offers),
            interval,
            hold,
            &mut shutdown,
            &mut pause,
        )
        .await?;
        match outcome {
            PollLoop::Offer(found) => self.execute(executor, found).await,
            PollLoop::Paused => Ok(RunDisposition::PollingPaused),
            PollLoop::Shutdown => Ok(RunDisposition::ShutdownBeforeAdmission),
        }
    }
    async fn poll_once(&self, offers: bool) -> Result<Option<Found>> {
        if let Some(h) = &self.heartbeat {
            h.check()?;
        }
        let offers = offers && self.backend_ready().await;
        let _inventory = self.pipeline.inventory.gate.lock().await;
        let need = self
            .pipeline
            .inventory
            .poll_need(self.config.inventory_ttl_ms);
        let sequence = node_inventory(
            &self.pipeline,
            &self.producer,
            self.config.inventory_ttl_ms,
            need,
        )
        .await?;
        if !offers || *self.shutdown.borrow() {
            return Ok(None);
        }
        let available = self.pipeline.available().await?;
        let mut page = self
            .client
            .poll(&Poll {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                identity: self.identity.clone(),
                inventory_sequence: sequence,
                available: Resources {
                    slots: available.slots,
                    memory_bytes: available.memory_bytes,
                    cpu_millis: available.cpu_millis,
                    disk_bytes: available.disk_bytes,
                },
                limit: 1,
                after_offer: None,
            })
            .await?;
        ensure!(
            page.page.offers.len() <= 1 && page.page.offers.len() == page.offer_deadlines_ns.len(),
            "native poll cardinality"
        );
        let found = page.page.offers.pop().zip(page.offer_deadlines_ns.pop());
        // From here a renewal of the node's report waits for this slot (see `poll_need`).
        Ok(found.map(|(offer, deadline)| {
            let git_author = page.git_authors.pop().flatten();
            let workspace_required = page.workspace_required.pop().unwrap_or(false);
            (
                aq_worker_protocol::git_author::AttemptOffer { offer, git_author },
                workspace_required,
                deadline,
                self.pipeline.inventory.flight(),
            )
        }))
    }
    pub(super) async fn backend_ready(&self) -> bool {
        let readiness = self.config.check_backend_admission().await;
        aq_node_service::layout::report_admission_wait(&self.config.state_dir, &readiness);
        readiness.is_ok()
    }
}
