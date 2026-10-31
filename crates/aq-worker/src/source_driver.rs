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

struct Control {
    attempt: Uuid,
    preparing: bool,
    next_poll: u64,
    next_renew: u64,
    renewal: Option<RenewRequest>,
}
impl Control {
    async fn exchange<C: Clock, F: FnMut() -> Fut, Fut: Future<Output = Result<T>>, T>(
        &mut self,
        runtime: &mut Runtime<C>,
        control: &ExecutionClient,
        mut operation: F,
    ) -> Result<T> {
        for attempt in 0..3 {
            match self.wait(runtime, control, operation()).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let remote = error.downcast_ref::<TransferHttpError>();
                    let transient = error
                        .downcast_ref::<RemoteError>()
                        .is_none_or(|e| e.error.retryable)
                        && remote.is_none_or(|e| {
                            e.status.is_server_error() || matches!(e.status.as_u16(), 408 | 429)
                        });
                    let delay = remote
                        .and_then(|e| e.retry_after_secs)
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| Duration::from_millis(250 << attempt));
                    let stopped = self.preparing
                        && !matches!(
                            runtime.journal.state().attempts[&self.attempt].phase,
                            Phase::Authorized | Phase::Preparing
                        );
                    if attempt == 2 || !transient || stopped || delay > Duration::from_secs(5) {
                        return Err(error);
                    }
                    self.wait(runtime, control, async {
                        tokio::time::sleep(delay).await;
                        Ok(())
                    })
                    .await?;
                }
            }
        }
        unreachable!("finite transfer attempt loop returns")
    }
    fn new(attempt: Uuid, preparing: bool) -> Self {
        Self {
            attempt,
            preparing,
            next_poll: 0,
            next_renew: 0,
            renewal: None,
        }
    }
    async fn tick<C: Clock>(
        &mut self,
        runtime: &mut Runtime<C>,
        client: &ExecutionClient,
    ) -> Result<()> {
        let now = runtime.clock.now_ms();
        if self.preparing {
            runtime.expire()?;
            let r = &runtime.journal.state().attempts[&self.attempt];
            ensure!(
                !runtime.journal.state().reconciliation_required
                    && matches!(r.phase, Phase::Authorized | Phase::Preparing)
                    && r.offer.identity == runtime.journal.state().identity
                    && now
                        < r.grant_deadline_ms
                            .context("source preparation grant absent")?,
                "source preparation stopped or expired; bytes retained"
            );
        }
        if now >= self.next_poll {
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
            match tokio::time::timeout(Duration::from_millis(100), client.poll(&request)).await {
                Ok(Ok(response)) => {
                    for directive in response.directives {
                        let ack = runtime.receive_directive(&directive)?;
                        let _ = tokio::time::timeout(
                            Duration::from_millis(100),
                            client.directive_ack(&ack),
                        )
                        .await;
                    }
                }
                Ok(Err(error)) => {
                    if let Some(remote) = error.downcast_ref::<RemoteError>() {
                        // Recovery of already stopped source may precede boot
                        // replacement. A stale poll cannot authorize execution.
                        if !remote.error.retryable
                            && (self.preparing || remote.error.code != ErrorCode::StaleBoot)
                        {
                            return Err(error);
                        }
                    }
                }
                Err(_) => {}
            }
            self.next_poll = runtime.clock.now_ms().saturating_add(250);
        }
        if self.preparing {
            runtime.expire()?;
            let r = &runtime.journal.state().attempts[&self.attempt];
            ensure!(
                matches!(r.phase, Phase::Authorized | Phase::Preparing),
                "source preparation cancelled or expired; bytes retained"
            );
            let now = runtime.clock.now_ms();
            if now >= self.next_renew {
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
                    client.renew(request),
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
            }
        }
        Ok(())
    }
    async fn wait<C: Clock, F: Future<Output = Result<T>>, T>(
        &mut self,
        runtime: &mut Runtime<C>,
        control: &ExecutionClient,
        future: F,
    ) -> Result<T> {
        tokio::pin!(future);
        loop {
            self.tick(runtime, control).await?;
            tokio::select! {
                result=&mut future=>return result,
                _=tokio::time::sleep(Duration::from_millis(50))=>{},
            }
        }
    }
}

pub(crate) async fn download_base<C: Clock>(
    runtime: &mut Runtime<C>,
    control: &ExecutionClient,
    transport: &TransferClient,
    id: Uuid,
    git: &GitClient,
) -> Result<()> {
    let mut pump = Control::new(id, true);
    if runtime.journal.state().attempts[&id]
        .source
        .as_ref()
        .unwrap()
        .download
        .is_none()
    {
        let manifest = pump
            .exchange(runtime, control, || transport.base(id))
            .await?;
        runtime.prepare_base_download(id, manifest, git)?;
    }
    while let Some(action) = runtime.next_base_download(id)? {
        match action {
            DownloadAction::Range(range) => {
                let bytes = pump
                    .exchange(runtime, control, || range.execute(transport))
                    .await?;
                runtime.store_base_download_chunk(id, &range, &bytes)?;
            }
            DownloadAction::Verify(verification) => {
                let verified = pump
                    .wait(runtime, control, async move {
                        tokio::task::spawn_blocking(move || verification.verify())
                            .await
                            .map_err(|_| anyhow::anyhow!("source verification task failed"))?
                    })
                    .await?;
                runtime.acknowledge_base_verification(id, verified)?;
            }
        }
    }
    pump.tick(runtime, control).await?;
    let artifact = runtime.journal.state().attempts[&id]
        .source
        .as_ref()
        .unwrap()
        .download
        .as_ref()
        .unwrap()
        .manifest
        .artifact_id;
    runtime.import_base(id, artifact, git)
}

pub(crate) async fn upload_source<C: Clock>(
    runtime: &mut Runtime<C>,
    control: &ExecutionClient,
    transport: &TransferClient,
    id: Uuid,
    git: &GitClient,
) -> Result<()> {
    runtime.prepare_source_upload(id, git)?;
    let mut pump = Control::new(id, false);
    // A failed exchange returns with durable intent intact. The driver's normal
    // restart/recovery path retries that exact action, without renewing a writer.
    while let Some(action) = runtime.next_source_upload(id)? {
        let status = pump
            .exchange(runtime, control, || action.execute(transport))
            .await?;
        runtime.acknowledge_source_upload(id, &action, status)?;
    }
    Ok(())
}
