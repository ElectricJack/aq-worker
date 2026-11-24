//! Minimal outbound synthetic-execution driver. All acceptance/report retries
//! reuse journaled bytes. It intentionally refuses uncertain restart relaunch.
use crate::{
    clock::{BootClock, Clock},
    config::WorkerConfig,
    execution_client::{ExecutionClient, RemoteError},
    journal::{Journal, Limits},
    runtime::Runtime,
    source_driver::SourceTransfer,
};
use anyhow::{Context, Result, bail, ensure};
use aq_worker_protocol::*;
use std::{path::Path, time::Duration};
use uuid::Uuid;

pub struct SourceOptions {
    pub transfer: bool,
    pub binary: std::path::PathBuf,
    pub binary_sha256: String,
    pub state_dir: std::path::PathBuf,
    pub git_executable: std::path::PathBuf,
    pub max_bundle_bytes: u64,
    pub max_expanded_bytes: u64,
}
pub struct DriverOptions {
    pub source: Option<SourceOptions>,
    pub max_agents: u32,
    pub idle_timeout_secs: u64,
    pub exit_after_result: bool,
    pub drop_acceptance_response_once: bool,
    pub drop_report_response_once: bool,
}
fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RemoteError>()
        .is_none_or(|e| e.error.retryable)
}
async fn retry_delay() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

mod admission;
mod attempt;
mod control;
mod recovery;
mod recovery_source;
mod reporting;
use reporting::{deliver_stopped_observation, retire_local, submit_report};
struct Driver<'a> {
    runtime: Runtime<BootClock>,
    client: &'a ExecutionClient,
    git: Option<&'a crate::source::GitClient>,
    transfer: Option<SourceTransfer<'a>>,
    executable: &'a Path,
    options: DriverOptions,
}
pub async fn run_execution(
    config: &WorkerConfig,
    state_dir: &Path,
    executable: &Path,
    options: DriverOptions,
) -> Result<()> {
    crate::guardian::probe_profile()?;
    let git_service = options
        .source
        .as_ref()
        .map(SourceOptions::start)
        .transpose()?;
    let git = git_service.as_ref().map(|s| &s.client);
    let existing_journal = state_dir.join("journal.json").exists();
    let identity = NodeIdentity {
        node_id: config.node_id,
        boot_id: Uuid::new_v4(),
    };
    let limits = options.limits(git.is_some());
    let runtime = Runtime::new(
        Journal::open(state_dir, identity.clone(), limits)?,
        BootClock,
    );
    let client = ExecutionClient::from_config(config)?;
    let transfer = options
        .source
        .as_ref()
        .filter(|s| s.transfer)
        .map(|_| crate::transfer_client::TransferClient::from_config(config, 2))
        .transpose()?;
    let source_transfer = git
        .zip(transfer.as_ref())
        .map(|(git, transport)| SourceTransfer {
            git,
            control: &client,
            transport,
        });
    let registration = registration(identity, git.is_some(), transfer.is_some());
    let mut driver = Driver {
        runtime,
        client: &client,
        git,
        transfer: source_transfer,
        executable,
        options,
    };
    // Recovery may precede explicit replacement of the old boot registration.
    if existing_journal && driver.recover(&registration).await? {
        return Ok(());
    }
    if driver.runtime.journal.state().local_drain {
        println!(
            "{}",
            serde_json::json!({"event":"drained","protected_usage":driver.runtime.journal.protected_usage()?,"available":driver.runtime.journal.available()?})
        );
        return Ok(());
    }
    driver.register(&registration).await?;
    driver.poll(&registration.identity).await
}
impl SourceOptions {
    fn start(&self) -> Result<crate::source::GitService> {
        let git = crate::source::GitService::start_with_git(
            &self.binary,
            &self.binary_sha256,
            &self.state_dir,
            &self.git_executable,
        )?;
        git.client
            .validate_storage_profile(self.max_bundle_bytes, self.max_expanded_bytes)?;
        Ok(git)
    }
}
impl DriverOptions {
    fn limits(&self, with_source: bool) -> Limits {
        let capacity = Resources {
            slots: self.max_agents,
            cpu_millis: 1_000_000,
            memory_bytes: 64 * 1024 * 1024 * 1024,
            disk_bytes: if with_source {
                8 * 1024 * 1024 * 1024
            } else {
                1024 * 1024 * 1024
            },
        };
        Limits {
            capacity,
            max_pending_results: 64,
            max_pending_bytes: if with_source {
                256 * 1024 * 1024
            } else {
                1024 * 1024
            },
            max_preparing: self.max_agents as usize,
            minimum_free_disk_bytes: crate::journal::MINIMUM_FREE_DISK_BYTES,
        }
    }
}
fn registration(identity: NodeIdentity, with_source: bool, with_transfer: bool) -> RegisterRequest {
    let mut registration = RegisterRequest {
        identity,
        operation_id: Uuid::new_v4(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        worker_version: env!("AQ_WORKER_VERSION").into(),
        capabilities: vec![
            FAKE_HARNESS_CAPABILITY.into(),
            crate::guardian::PROFILE.into(),
        ],
    };
    if with_source {
        registration.capabilities.push(SOURCE_CAPABILITY.into());
    }
    if with_transfer {
        registration
            .capabilities
            .push(SOURCE_TRANSFER_CAPABILITY.into());
    }
    registration
}
impl Driver<'_> {
    async fn register(&self, registration: &RegisterRequest) -> Result<()> {
        let identity = &registration.identity;
        let registered = match self.client.register(registration).await {
            Ok(r) => r,
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"event":"incarnation_replacement_required","registration":registration})
                );
                return Err(error);
            }
        };
        ensure!(
            &registered.identity == identity
                && registered
                    .capabilities
                    .contains(&FAKE_HARNESS_CAPABILITY.into()),
            "coordinator negotiated different identity/capabilities"
        );
        println!(
            "{}",
            serde_json::json!({"event":"registered","identity":identity})
        );
        Ok(())
    }
    fn idle_deadline(&self) -> u64 {
        self.runtime
            .clock
            .now_ms()
            .saturating_add(self.options.idle_timeout_secs.saturating_mul(1000))
    }
    async fn poll(&mut self, identity: &NodeIdentity) -> Result<()> {
        let mut idle_deadline = self.idle_deadline();
        loop {
            ensure!(
                self.runtime.clock.now_ms() < idle_deadline,
                "execution idle timeout"
            );
            let poll = availability_poll(identity.clone(), &mut self.runtime)?;
            let response = match self.client.poll(&poll).await {
                Ok(r) => r,
                Err(e) if retryable(&e) => {
                    retry_delay().await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if self
                .process_poll(identity, response, &mut idle_deadline)
                .await?
            {
                return Ok(());
            }
            retry_delay().await;
        }
    }
    async fn process_poll(
        &mut self,
        identity: &NodeIdentity,
        response: PollResponse,
        idle_deadline: &mut u64,
    ) -> Result<bool> {
        self.ack_directives(&response.directives).await?;
        for offer in response.offers {
            match self
                .handle_offer(offer, identity, response.server_time_ms)
                .await?
            {
                admission::Progress::Skip => {}
                admission::Progress::Finished => return Ok(true),
                admission::Progress::Acknowledged => {
                    *idle_deadline = self.idle_deadline();
                    if self.options.exit_after_result || self.runtime.journal.state().local_drain {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
    async fn ack_directives(&mut self, directives: &[Directive]) -> Result<()> {
        for directive in directives {
            let ack = self.runtime.receive_directive(directive)?;
            let result = self.client.directive_ack(&ack).await?;
            ensure!(
                result.operation_id == ack.operation_id,
                "directive ack mismatch"
            );
        }
        Ok(())
    }
}
fn availability_poll(
    identity: NodeIdentity,
    runtime: &mut Runtime<BootClock>,
) -> Result<PollRequest> {
    let inventory_sequence = runtime.journal.transaction(|s| {
        s.inventory_sequence = s
            .inventory_sequence
            .checked_add(1)
            .context("inventory overflow")?;
        Ok(s.inventory_sequence)
    })?;
    Ok(PollRequest {
        identity,
        operation_id: Uuid::new_v4(),
        inventory_sequence,
        available: runtime.journal.available()?,
        max_items: MAX_BATCH as u32,
    })
}
