//! Live-owner-only literal output. No renderer, input, snapshots or authority.
use super::*;
use aq_session_host::terminal_output::ControlEvent;
use aq_session_protocol::{OutputGap, TerminalIdentity, TerminalPolicy, Text};
use aq_terminal_bridge::{
    http::RelayHttp,
    output::{OutputTails, TailState},
};
use aq_terminal_relay_protocol::{self as wire, Peer, PeerUpload, Register, UploadAck};
use std::sync::Weak;
mod started;
use started::RetainedStarted;

// Match RelayHttp::output_upload's fixed wire-body limit. JSON byte arrays
// need up to four bytes per raw byte, plus the fixed identity/envelope fields.
const MAX_UPLOAD: usize = 256 * 1024;
const MAX_LOCAL_FRAME_BYTES: u32 = ((MAX_UPLOAD - 16 * 1024) / 4) as u32;
fn local_framing(policy: &TerminalPolicy) -> TerminalPolicy {
    let mut local = policy.clone();
    local.max_frame_bytes = local.max_frame_bytes.min(MAX_LOCAL_FRAME_BYTES);
    local
}
#[derive(Default)]
pub(super) struct Outputs {
    tails: OutputTails,
    owners: BTreeMap<Uuid, Arc<Relay>>,
}
struct Relay {
    terminal: TerminalIdentity,
    attempt: Uuid,
    journal: Weak<Mutex<Owned>>,
    peer: Peer,
    started: RetainedStarted,
    policy: TerminalPolicy,
    network: tokio::sync::Mutex<Network>,
    draining: AtomicBool,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}
struct Network {
    http: RelayHttp,
    registration: Option<Register>,
    info: Option<aq_terminal_relay::api::TerminalInfo>,
    through: u64,
    pending: Option<Pending>,
}
struct Pending {
    request: PeerUpload,
    digest: Digest,
    through: u64,
}
impl Outputs {
    pub(super) fn attached(&self, id: Uuid) -> bool {
        self.owners.contains_key(&id)
    }
}
impl Relay {
    fn registration(&self) -> Result<Register> {
        let journal = self
            .journal
            .upgrade()
            .context("native output journal owner dropped")?;
        let owner = journal
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native output journal busy"))?;
        ensure!(
            !owner.journal.recovery_required(),
            "native output restart is history-only"
        );
        let attempt = owner
            .journal
            .state()
            .attempts
            .get(&self.attempt)
            .context("native output retained attempt absent")?;
        let history = attempt
            .native_host
            .as_ref()
            .context("native output retained host absent")?;
        let current = RetainedStarted::capture(attempt)?;
        self.started.ensure_same(&current)?;
        let ack = self.started.acknowledgment(history)?;
        ensure!(
            history.intent.terminal == self.terminal,
            "native output retained terminal changed"
        );
        let q = Register {
            version: wire::VERSION,
            capability: Text::new(wire::CAPABILITY)?,
            operation_id: ack.operation_id,
            peer: self.peer.clone(),
            terminal: self.terminal.clone(),
            expected_authority_revision: ack.authority_revision_at_record,
            input_mode: wire::InputMode::FiniteV1,
            advertised_capabilities: vec![aq_session_protocol::Capability {
                id: Text::new(wire::CAPABILITY)?,
                version: wire::VERSION,
            }],
        };
        q.validate()?;
        Ok(q)
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort()
        }
    }
}
/// A process-local handle. Dropping a waiter does not stop the registry-owned
/// drain; dropping the owning pipeline aborts it. It cannot be deserialized.
pub struct PhaseOutputRelay {
    owner: Arc<Relay>,
    outputs: Weak<Mutex<Outputs>>,
}
#[derive(Clone, Copy, Debug)]
pub struct PhaseOutputStatus {
    pub next_sequence: u64,
    pub retained_bytes: usize,
    pub discontinuous: bool,
    pub draining: bool,
}
impl PhaseOutputRelay {
    pub fn terminal(&self) -> &TerminalIdentity {
        &self.owner.terminal
    }
    pub fn status(&self) -> Result<PhaseOutputStatus> {
        let outputs = self
            .outputs
            .upgrade()
            .context("native output owner dropped")?;
        let mut outputs = lock(&outputs)?;
        let tail = outputs.tails.tail_mut(&self.owner.terminal)?;
        Ok(PhaseOutputStatus {
            next_sequence: tail.next_sequence(),
            retained_bytes: tail.retained_bytes(),
            discontinuous: tail.state() == TailState::Discontinuous,
            draining: self.owner.draining.load(Ordering::SeqCst),
        })
    }
    /// Exact registration readiness only. No watch/buffer, input or execution
    /// permission is created by this returned metadata.
    pub async fn register(&self) -> Result<aq_terminal_relay::api::TerminalInfo> {
        ensure!(
            self.outputs.upgrade().is_some(),
            "native output owner dropped"
        );
        let mut network = self
            .owner
            .network
            .try_lock()
            .context("native output upload busy")?;
        register(&self.owner, &mut network).await
    }
    /// One explicit bounded exchange. Refusal/lost response preserves this exact
    /// pending request; network activity never holds the tail or guardian locks.
    pub async fn upload_once(&self) -> Result<Option<UploadAck>> {
        ensure!(
            self.outputs.upgrade().is_some(),
            "native output owner dropped"
        );
        let mut network = self
            .owner
            .network
            .try_lock()
            .context("native output upload busy")?;
        register(&self.owner, &mut network).await?;
        if network.pending.is_none() {
            let outputs = self
                .outputs
                .upgrade()
                .context("native output owner dropped")?;
            let mut outputs = lock(&outputs)?;
            network.pending = prepare(
                outputs.tails.tail_mut(&self.owner.terminal)?,
                &self.owner.peer,
                network.through,
            )?;
        }
        let Some(pending) = network.pending.as_ref() else {
            return Ok(None);
        };
        ensure!(
            Digest::of(&serde_json::to_vec(&pending.request)?) == pending.digest,
            "native output pending bytes changed"
        );
        let ack = network.http.output_upload(&pending.request).await?;
        ensure!(
            ack.dropped_output.iter().all(|g| {
                let count = g
                    .last_missing
                    .checked_sub(g.first_missing)
                    .and_then(|n| n.checked_add(1));
                g.terminal == self.owner.terminal
                    && g.first_missing > network.through
                    && count.is_some_and(|n| {
                        n <= 64
                            && pending
                                .request
                                .frames
                                .iter()
                                .filter(|f| {
                                    f.sequence >= g.first_missing && f.sequence <= g.last_missing
                                })
                                .count() as u64
                                == n
                    })
            }),
            "native output foreign drop range"
        );
        network.through = pending.through;
        network.pending = None;
        Ok(Some(ack))
    }
}
async fn register(
    owner: &Relay,
    network: &mut Network,
) -> Result<aq_terminal_relay::api::TerminalInfo> {
    if let Some(info) = &network.info {
        return Ok(info.clone());
    }
    if network.registration.is_none() {
        network.registration = Some(owner.registration()?)
    }
    let info = network
        .http
        .register(network.registration.as_ref().unwrap(), &owner.policy)
        .await?;
    network.info = Some(info.clone());
    Ok(info)
}

fn prepare(
    tail: &aq_terminal_bridge::output::OutputTail,
    peer: &Peer,
    through: u64,
) -> Result<Option<Pending>> {
    let mut q = PeerUpload {
        version: wire::VERSION,
        capability: Text::new(wire::CAPABILITY)?,
        peer: peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: 0,
        receipts: vec![],
        epoch_receipts: vec![],
        resize_receipts: vec![],
        fence_acks: vec![],
        frames: vec![],
        snapshots: vec![],
        gaps: vec![],
    };
    let mut high = through;
    for gap in tail.gaps().filter(|g| g.last_missing > through).take(64) {
        let g = OutputGap {
            terminal: gap.terminal.clone(),
            first_missing: gap.first_missing.max(through.saturating_add(1)),
            last_missing: gap.last_missing,
        };
        q.gaps.push(g.clone());
        if serde_json::to_vec(&q)?.len() > MAX_UPLOAD {
            q.gaps.pop();
            break;
        }
        high = high.max(g.last_missing);
    }
    for frame in tail.frames().filter(|f| f.sequence > through).take(64) {
        q.frames.push(frame.clone());
        if serde_json::to_vec(&q)?.len() > MAX_UPLOAD {
            q.frames.pop();
            break;
        }
        high = high.max(frame.sequence);
    }
    if q.frames.is_empty() && q.gaps.is_empty() {
        return Ok(None);
    }
    q.validate()?;
    let bytes = serde_json::to_vec(&q)?;
    ensure!(bytes.len() <= MAX_UPLOAD, "native output upload bytes");
    Ok(Some(Pending {
        request: q,
        digest: Digest::of(&bytes),
        through: high,
    }))
}
impl InputPipeline {
    /// Start draining genuine local Running immediately. Upload waits for its actual
    /// persisted Started ACK. Repeated attachment refuses; reconnect keeps this handle.
    pub fn start_phase_output(
        self: &Arc<Self>,
        host: &LocalPhaseHost,
        http: RelayHttp,
    ) -> Result<PhaseOutputRelay> {
        ensure!(
            Arc::ptr_eq(&self.owned, &host.owner)
                && Arc::ptr_eq(&self.launch.entry(host.entry.id)?, &host.entry),
            "native output foreign host"
        );
        let runtime = tokio::runtime::Handle::try_current()
            .context("native output requires async runtime")?;
        let owner = self
            .owned
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native output journal busy"))?;
        host.entry.current(&owner)?;
        let a = owner
            .journal
            .state()
            .attempts
            .get(&host.entry.id)
            .context("native output attempt absent")?;
        let history = a
            .native_host
            .as_ref()
            .context("native output host history absent")?;
        let terminal = history.intent.terminal.clone();
        let policy = a.reservation.offer.offered.offer.manifest.terminal.clone();
        ensure!(
            !policy.input_enabled,
            "native output-only sealed policy required"
        );
        let started_request = RetainedStarted::capture(a)?;
        ensure!(
            history.running.is_some() && !host.entry.stop.load(Ordering::SeqCst),
            "native output Started identity or phase mismatch"
        );
        let peer = Peer::Remote {
            node_id: owner.journal.state().identity.node_id,
            boot_id: owner.journal.state().identity.boot_id,
        };
        drop(owner);
        let mut outputs = lock(&self.launch.output)?;
        ensure!(
            !outputs.owners.contains_key(&host.entry.id),
            "native output already attached; no sequence reset"
        );
        outputs
            .tails
            .attach(terminal.clone(), &local_framing(&policy))?;
        let relay = Arc::new(Relay {
            terminal: terminal.clone(),
            attempt: host.entry.id,
            journal: Arc::downgrade(&self.owned),
            peer,
            started: started_request,
            policy,
            network: tokio::sync::Mutex::new(Network {
                http,
                registration: None,
                info: None,
                through: 0,
                pending: None,
            }),
            draining: AtomicBool::new(true),
            task: Mutex::new(None),
        });
        outputs.owners.insert(host.entry.id, relay.clone());
        drop(outputs);
        let weak_outputs = Arc::downgrade(&self.launch.output);
        let weak_entry = Arc::downgrade(&host.entry);
        let weak_relay = Arc::downgrade(&relay);
        let task = runtime.spawn(async move { drain(weak_outputs, weak_entry, weak_relay).await });
        *lock(&relay.task)? = Some(task.abort_handle());
        Ok(PhaseOutputRelay {
            owner: relay,
            outputs: Arc::downgrade(&self.launch.output),
        })
    }
}
async fn drain(outputs: Weak<Mutex<Outputs>>, entry: Weak<Entry>, relay: Weak<Relay>) {
    let mut timer = tokio::time::interval(Duration::from_millis(10));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        let (Some(outputs), Some(entry), Some(relay)) =
            (outputs.upgrade(), entry.upgrade(), relay.upgrade())
        else {
            return;
        };
        let stop = {
            let Ok(mut outputs) = outputs.try_lock() else {
                continue;
            };
            let Ok(mut physical) = entry.physical.try_lock() else {
                continue;
            };
            let stopped = matches!(physical.outcome, Outcome::Stopped | Outcome::Unknown);
            if physical.observer.is_none() {
                relay.draining.store(false, Ordering::SeqCst);
                return;
            }
            let Ok(tail) = outputs.tails.tail_mut(&relay.terminal) else {
                relay.draining.store(false, Ordering::SeqCst);
                return;
            };
            match physical.poll_output() {
                Ok(events) => {
                    let empty = events.is_empty();
                    for event in events {
                        if tail.state() != TailState::Discontinuous && tail.accept(&event).is_err()
                        {
                            let _ = tail.accept(&ControlEvent::CommandError);
                        }
                    }
                    stopped && empty
                }
                Err(_) => {
                    let _ = tail.accept(&ControlEvent::CommandError);
                    stopped
                }
            }
        };
        if stop {
            relay.draining.store(false, Ordering::SeqCst);
            return;
        }
    }
}

#[cfg(all(test, feature = "test-support"))]
#[path = "output_relay_tests.rs"]
mod tests;
