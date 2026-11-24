//! Source transfer integration for the synthetic driver. No execution authority
//! is obtained from transfer completion. Each exchange yields to control polls.
use crate::{
    clock::Clock,
    execution_client::{ExecutionClient, RemoteError},
    runtime::Runtime,
    source::GitClient,
    source_transfer::DownloadAction,
    transfer_client::{TransferClient, TransferHttpError},
};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::*;
use std::{future::Future, time::Duration};
use uuid::Uuid;

struct Control<'a> {
    client: &'a ExecutionClient,
    attempt: Uuid,
    preparing: bool,
    next_poll: u64,
    next_renew: u64,
    renewal: Option<RenewRequest>,
}
fn retry_delay(error: &anyhow::Error, attempt: u32) -> Option<Duration> {
    let remote = error.downcast_ref::<TransferHttpError>();
    let transient = error
        .downcast_ref::<RemoteError>()
        .is_none_or(|e| e.error.retryable)
        && remote
            .is_none_or(|e| e.status.is_server_error() || matches!(e.status.as_u16(), 408 | 429));
    let delay = remote
        .and_then(|e| e.retry_after_secs)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_millis(250 << attempt));
    (transient && delay <= Duration::from_secs(5)).then_some(delay)
}

impl<'a> Control<'a> {
    async fn exchange<C: Clock, F: FnMut() -> Fut, Fut: Future<Output = Result<T>>, T>(
        &mut self,
        runtime: &mut Runtime<C>,
        mut operation: F,
    ) -> Result<T> {
        for attempt in 0..3 {
            match self.wait(runtime, operation()).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let delay = retry_delay(&error, attempt);
                    let stopped = self.preparing && !self.active(runtime);
                    let Some(delay) = delay.filter(|_| !stopped && attempt < 2) else {
                        return Err(error);
                    };
                    self.wait(runtime, async {
                        tokio::time::sleep(delay).await;
                        Ok(())
                    })
                    .await?;
                }
            }
        }
        unreachable!("finite transfer attempt loop returns")
    }
    fn new(client: &'a ExecutionClient, attempt: Uuid, preparing: bool) -> Self {
        Self {
            client,
            attempt,
            preparing,
            next_poll: 0,
            next_renew: 0,
            renewal: None,
        }
    }
    fn active<C: Clock>(&self, runtime: &Runtime<C>) -> bool {
        matches!(
            runtime.journal.state().attempts[&self.attempt].phase,
            Phase::Authorized | Phase::Preparing
        )
    }
    async fn tick<C: Clock>(&mut self, runtime: &mut Runtime<C>) -> Result<()> {
        let now = runtime.clock.now_ms();
        if self.preparing {
            self.check_grant(runtime, now)?;
        }
        if now >= self.next_poll {
            self.poll(runtime).await?;
        }
        if self.preparing {
            self.renew(runtime).await?;
        }
        Ok(())
    }
    fn check_grant<C: Clock>(&self, runtime: &mut Runtime<C>, now: u64) -> Result<()> {
        runtime.expire()?;
        let r = &runtime.journal.state().attempts[&self.attempt];
        ensure!(
            !runtime.journal.state().reconciliation_required
                && self.active(runtime)
                && r.offer.identity == runtime.journal.state().identity
                && now
                    < r.grant_deadline_ms
                        .context("source preparation grant absent")?,
            "source preparation stopped or expired; bytes retained"
        );
        Ok(())
    }
    async fn poll<C: Clock>(&mut self, runtime: &mut Runtime<C>) -> Result<()> {
        let sequence = runtime.journal.transaction(|s| {
            s.inventory_sequence = s
                .inventory_sequence
                .checked_add(1)
                .context("inventory exhausted")?;
            Ok(s.inventory_sequence)
        })?;
        let request = PollRequest {
            identity: runtime.journal.state().identity.clone(),
            operation_id: Uuid::new_v4(),
            inventory_sequence: sequence,
            available: Resources {
                slots: 0,
                cpu_millis: 0,
                memory_bytes: 0,
                disk_bytes: 0,
            },
            max_items: MAX_BATCH as u32,
        };
        match tokio::time::timeout(Duration::from_millis(100), self.client.poll(&request)).await {
            Ok(Ok(response)) => {
                for directive in response.directives {
                    let ack = runtime.receive_directive(&directive)?;
                    let _ = tokio::time::timeout(
                        Duration::from_millis(100),
                        self.client.directive_ack(&ack),
                    )
                    .await;
                }
            }
            Ok(Err(error)) if self.poll_refused(&error) => return Err(error),
            _ => {}
        }
        self.next_poll = runtime.clock.now_ms().saturating_add(250);
        Ok(())
    }
    // Recovery of stopped source may precede boot replacement.
    fn poll_refused(&self, error: &anyhow::Error) -> bool {
        let Some(remote) = error.downcast_ref::<RemoteError>() else {
            return false;
        };
        let stale_recovery = !self.preparing && remote.error.code == ErrorCode::StaleBoot;
        !remote.error.retryable && !stale_recovery
    }
    async fn renew<C: Clock>(&mut self, runtime: &mut Runtime<C>) -> Result<()> {
        runtime.expire()?;
        let r = &runtime.journal.state().attempts[&self.attempt];
        ensure!(
            self.active(runtime),
            "source preparation cancelled or expired; bytes retained"
        );
        let now = runtime.clock.now_ms();
        if now < self.next_renew {
            return Ok(());
        }
        let request = self.renewal.get_or_insert_with(|| RenewRequest {
            identity: runtime.journal.state().identity.clone(),
            operation_id: Uuid::new_v4(),
            attempt: r.offer.attempt.clone(),
            grant_sequence: r.grant.as_ref().unwrap().grant_sequence,
        });
        let remaining = r.grant_deadline_ms.unwrap().saturating_sub(now);
        ensure!(remaining > 0, "source preparation authority expired");
        match tokio::time::timeout(
            Duration::from_millis(remaining.min(100)),
            self.client.renew(request),
        )
        .await
        {
            Ok(Ok(response)) => {
                runtime.renew(response.grant, now)?;
                self.renewal = None;
                self.next_renew = runtime.clock.now_ms().saturating_add(
                    runtime.journal.state().attempts[&self.attempt]
                        .grant_deadline_ms
                        .unwrap()
                        .saturating_sub(runtime.clock.now_ms())
                        / 3,
                );
            }
            Ok(Err(error))
                if error
                    .downcast_ref::<RemoteError>()
                    .is_some_and(|e| !e.error.retryable) =>
            {
                return Err(error);
            }
            _ => {
                self.next_renew = now.saturating_add(100);
            }
        }
        Ok(())
    }
    async fn wait<C: Clock, F: Future<Output = Result<T>>, T>(
        &mut self,
        runtime: &mut Runtime<C>,
        future: F,
    ) -> Result<T> {
        tokio::pin!(future);
        loop {
            self.tick(runtime).await?;
            tokio::select! {
                result=&mut future=>return result,
                _=tokio::time::sleep(Duration::from_millis(50))=>{},
            }
        }
    }
}

pub(crate) struct SourceTransfer<'a> {
    pub git: &'a GitClient,
    pub control: &'a ExecutionClient,
    pub transport: &'a TransferClient,
}

impl SourceTransfer<'_> {
    pub(crate) async fn download_base<C: Clock>(
        &self,
        runtime: &mut Runtime<C>,
        id: Uuid,
    ) -> Result<()> {
        let mut pump = Control::new(self.control, id, true);
        if runtime.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .download
            .is_none()
        {
            let manifest = pump.exchange(runtime, || self.transport.base(id)).await?;
            runtime.prepare_base_download(id, manifest, self.git)?;
        }
        while let Some(action) = runtime.next_base_download(id)? {
            match action {
                DownloadAction::Range(range) => {
                    let bytes = pump
                        .exchange(runtime, || range.execute(self.transport))
                        .await?;
                    runtime.store_base_download_chunk(id, &range, &bytes)?;
                }
                DownloadAction::Verify(verification) => {
                    let verified = pump
                        .wait(runtime, async move {
                            tokio::task::spawn_blocking(move || verification.verify())
                                .await
                                .map_err(|_| anyhow::anyhow!("source verification task failed"))?
                        })
                        .await?;
                    runtime.acknowledge_base_verification(id, verified)?;
                }
            }
        }
        pump.tick(runtime).await?;
        let artifact = runtime.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .download
            .as_ref()
            .unwrap()
            .manifest
            .artifact_id;
        runtime.import_base(id, artifact, self.git)
    }

    pub(crate) async fn upload_source<C: Clock>(
        &self,
        runtime: &mut Runtime<C>,
        id: Uuid,
    ) -> Result<()> {
        runtime.prepare_source_upload(id, self.git)?;
        let mut pump = Control::new(self.control, id, false);
        // A failed exchange returns with durable intent intact. The driver's normal
        // restart/recovery path retries that exact action, without renewing a writer.
        while let Some(action) = runtime.next_source_upload(id)? {
            let status = pump
                .exchange(runtime, || action.execute(self.transport))
                .await?;
            runtime.acknowledge_source_upload(id, &action, status)?;
        }
        Ok(())
    }
}
