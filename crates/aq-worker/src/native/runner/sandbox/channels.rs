//! Per-sandbox terminal and observation channels (SBX16).
//!
//! Each sandbox holds at most one terminal long poll, under its own delegated
//! permit and B1 channel identity, on a connection pool separate from renewal
//! and cancellation. A poll is held (at most 20 s) only while the sandbox has
//! terminal subscriptions or controller work; an idle sandbox sends nothing.
//! Frames are bounded at 64 KiB. Per-node aggregates are enforced by the server.
use super::auth::{RunnerHttp, RunnerRoute, TerminalRoute};
use anyhow::{Context as _, Result, ensure};
use aq_admission_sandbox::sandbox::PermitBinding;
use aq_session_protocol::Validate;
use aq_session_protocol::{Capability, Text};
use aq_terminal_relay_protocol::{self as wire, Peer};
use serde::Serialize;
use tokio::sync::{Mutex, watch};

pub const MAX_FRAME: usize = 64 * 1024;
pub const HOLD_MS: u32 = 20_000;
const MAX_DEMAND: u32 = 32;
use super::output::SandboxOutput;
use crate::native::terminal_output::{MAX_SESSION_VIEWERS, TerminalOutput};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalDemand {
    pub subscriptions: u32,
    pub controllers: u32,
}
impl TerminalDemand {
    pub fn idle(&self) -> bool {
        self.subscriptions == 0 && self.controllers == 0
    }
}

pub struct SandboxChannels {
    http: RunnerHttp,
    binding: PermitBinding,
    peer: Peer,
    /// Held for the duration of the one terminal poll; also owns the cursor.
    cursor: Mutex<u64>,
    demand: watch::Sender<TerminalDemand>,
    output: OnceLock<Arc<SandboxOutput>>,
}
impl SandboxChannels {
    pub fn new(http: &RunnerHttp) -> Result<Self> {
        let binding = http.binding();
        http.check_scope(RunnerRoute::Terminal(TerminalRoute::Poll), &binding)?;
        http.check_scope(RunnerRoute::Observations, &binding)?;
        Ok(Self {
            http: http.channel_pool()?,
            peer: Peer::Remote {
                node_id: binding.node.node_id,
                boot_id: binding.node.boot_id,
            },
            binding,
            cursor: Mutex::new(0),
            demand: watch::Sender::new(TerminalDemand::default()),
            output: OnceLock::new(),
        })
    }
    pub(crate) fn validate_authority(&self, http: &RunnerHttp) -> Result<()> {
        ensure!(
            self.http.shares_authority(http),
            "terminal channels use another permit authority"
        );
        Ok(())
    }
    pub(super) fn authority_lost(&self) -> bool {
        self.http.authority_lost()
    }
    pub(super) async fn run_output(&self, output: Arc<SandboxOutput>) -> Result<()> {
        let mut demand = self.demand.subscribe();
        loop {
            demand.wait_for(|d| !d.idle()).await?;
            ensure!(!self.http.authority_lost(), "runner authority lost");
            if let Some(envelope) = self.poll_terminal().await? {
                for _ in envelope.effects {
                    output.request_snapshot();
                }
            }
        }
    }
    pub fn set_demand(&self, demand: TerminalDemand) -> Result<()> {
        ensure!(
            demand.subscriptions <= MAX_SESSION_VIEWERS as u32 && demand.controllers <= MAX_DEMAND,
            "terminal demand bound"
        );
        let prior = self.demand.send_replace(demand);
        if demand.subscriptions > prior.subscriptions
            && let Some(output) = self.output.get()
        {
            output.request_snapshot();
        }
        Ok(())
    }
    pub fn open_output(
        &self,
        baseline: &aq_session_host::observed::VerifiedBlankBaseline,
        policy: &aq_session_protocol::TerminalPolicy,
    ) -> Result<Arc<SandboxOutput>> {
        self.output_from(TerminalOutput::from_verified(baseline, policy)?)
    }
    pub(crate) fn output_from(&self, output: TerminalOutput) -> Result<Arc<SandboxOutput>> {
        let output = Arc::new(SandboxOutput::new(
            &self.http,
            output,
            self.demand.subscribe(),
        )?);
        self.output
            .set(output.clone())
            .map_err(|_| anyhow::anyhow!("sandbox output already attached; no epoch reuse"))?;
        Ok(output)
    }
    /// Whether this sandbox currently holds its terminal poll.
    pub fn terminal_held(&self) -> bool {
        self.cursor.try_lock().is_err()
    }
    /// One terminal long poll. Idle sends nothing; a second concurrent poll for
    /// the same sandbox is refused rather than queued.
    pub async fn poll_terminal(&self) -> Result<Option<wire::Envelope>> {
        let demand = *self.demand.borrow();
        if demand.idle() {
            return Ok(None);
        }
        let mut cursor = self
            .cursor
            .try_lock()
            .ok()
            .context("terminal poll already held for this sandbox")?;
        let request = wire::RelayPoll {
            version: wire::VERSION,
            capability: Text::new(wire::CAPABILITY)?,
            peer: self.peer.clone(),
            cursor: *cursor,
            hold_ms: HOLD_MS,
            active_subscriptions: demand.subscriptions,
            active_controllers: demand.controllers,
            advertised_capabilities: vec![Capability {
                id: Text::new(wire::CAPABILITY)?,
                version: wire::VERSION,
            }],
        };
        request.validate()?;
        let bytes = self
            .http
            .post_bounded(
                RunnerRoute::Terminal(TerminalRoute::Poll),
                &self.binding,
                &request,
                MAX_FRAME,
                MAX_FRAME,
            )
            .await?;
        let envelope = wire::decode(&bytes)?;
        ensure!(
            envelope.peer == self.peer && envelope.cursor >= *cursor,
            "terminal envelope peer or cursor"
        );
        if let Some(output) = self.output.get() {
            let terminal = output.terminal()?;
            for effect in &envelope.effects {
                ensure!(effect.terminal() == &terminal, "foreign output watch");
                ensure!(
                    matches!(effect, wire::Effect::Watch(_)),
                    "unsupported output effect"
                );
            }
        }
        *cursor = envelope.cursor;
        Ok(Some(envelope))
    }
    /// Long-poll loop: waits (sending nothing) while idle, then holds one poll
    /// at a time. The server answers at once when work exists.
    pub async fn run_terminal(&self, mut deliver: impl FnMut(wire::Envelope)) -> Result<()> {
        let mut demand = self.demand.subscribe();
        loop {
            demand.wait_for(|d| !d.idle()).await?;
            ensure!(!self.http.authority_lost(), "runner authority lost");
            if let Some(envelope) = self.poll_terminal().await? {
                deliver(envelope);
            }
        }
    }
    /// Observation batches use the same per-sandbox channel and permit.
    pub async fn upload_observations(&self, batch: &impl Serialize) -> Result<Vec<u8>> {
        self.http
            .post_bounded(
                RunnerRoute::Observations,
                &self.binding,
                batch,
                aq_observation::lifecycle::MAX_BATCH_BYTES,
                aq_observation::lifecycle::MAX_BATCH_BYTES,
            )
            .await
    }
    pub fn check_observation_scope(
        &self,
        binding: &aq_observation::lifecycle::Binding,
    ) -> Result<()> {
        binding.validate()?;
        ensure!(
            binding.session == self.http.launch_binding().session
                && binding.attempt_id == self.binding.attempt_id,
            "observation source outside sandbox permit"
        );
        Ok(())
    }
    pub async fn deliver_observations(
        &self,
        packet: &aq_observation::lifecycle::Packet,
    ) -> Result<aq_observation::lifecycle::Receipt> {
        use aq_observation::lifecycle::{Message, Validate};
        packet.validate()?;
        let Message::Batch(batch) = &packet.body.message else {
            anyhow::bail!("observation packet class")
        };
        ensure!(!batch.events.is_empty(), "empty observation batch");
        for event in &batch.events {
            self.check_observation_scope(&event.body.binding)?;
        }
        let bytes = self.upload_observations(packet).await?;
        let receipt: aq_observation::lifecycle::Receipt = serde_json::from_slice(&bytes)?;
        receipt.validate_against(packet)?;
        Ok(receipt)
    }
}
