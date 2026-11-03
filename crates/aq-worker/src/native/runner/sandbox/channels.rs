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
use aq_session_protocol::{Capability, Text};
use aq_terminal_relay_protocol::{self as wire, Peer};
use serde::Serialize;
use tokio::sync::{Mutex, watch};

pub const MAX_FRAME: usize = 64 * 1024;
pub const HOLD_MS: u32 = 20_000;
const MAX_DEMAND: u32 = 32;

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
        })
    }
    pub fn set_demand(&self, demand: TerminalDemand) -> Result<()> {
        ensure!(
            demand.subscriptions <= MAX_DEMAND && demand.controllers <= MAX_DEMAND,
            "terminal demand bound"
        );
        self.demand.send_replace(demand);
        Ok(())
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
            .post(RunnerRoute::Observations, &self.binding, batch)
            .await
    }
}
