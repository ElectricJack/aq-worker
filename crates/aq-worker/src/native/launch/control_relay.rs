//! Native phase ownership joined to the existing finite, durable terminal bridge.
//! One node-wide full-poll cursor prevents different attempts claiming each
//! other's input. Watch/output remains on its independent output-only transport.
use super::*;
use aq_session_host::terminal_io::{QualifiedClock, Receipt, TerminalBinding, TerminalHandle};
use aq_terminal_bridge::{Bridge, OwnedTerminal, http::RelayHttp};
use aq_terminal_relay_protocol::{Effect, Register};
use std::{path::PathBuf, sync::Weak};

struct GuardedTerminal {
    entry: Weak<Entry>,
    journal: Weak<Mutex<Owned>>,
    output: PhaseOutputRelay,
    registration: Register,
    terminal: TerminalHandle,
}
impl GuardedTerminal {
    fn current(&self) -> Result<Arc<Entry>> {
        let entry = self
            .entry
            .upgrade()
            .context("native terminal host dropped")?;
        let journal = self
            .journal
            .upgrade()
            .context("native terminal journal dropped")?;
        {
            let owner = lock(&journal)?;
            entry.current(&owner)?;
            let a = owner
                .journal
                .state()
                .attempts
                .get(&entry.id)
                .context("native terminal attempt absent")?;
            ensure!(
                a.reservation
                    .offer
                    .offered
                    .offer
                    .manifest
                    .terminal
                    .input_enabled,
                "native terminal input not sealed into original profile"
            );
            let history = a
                .native_host
                .as_ref()
                .context("native terminal history absent")?;
            ensure!(
                history.intent.terminal == self.terminal.binding().terminal,
                "native terminal incarnation changed"
            );
        }
        // This recaptures the current exact nominal Started ACK. A reopened
        // journal is history only and cannot produce a new input capability.
        ensure!(
            serde_json::to_vec(&self.output.control_registration()?)?
                == serde_json::to_vec(&self.registration)?,
            "native terminal original Started registration changed"
        );
        Ok(entry)
    }
    fn viewport(&self) -> Result<String> {
        let entry = self.current()?;
        let physical = lock(&entry.physical)?;
        ensure!(
            !entry.stop.load(Ordering::SeqCst) && matches!(physical.outcome, Outcome::Running),
            "native terminal stopped before viewport read"
        );
        self.terminal.current_viewport()
    }
}
impl OwnedTerminal for GuardedTerminal {
    fn binding(&self) -> &TerminalBinding {
        self.terminal.binding()
    }
    fn submit(&self, effect: &Effect, clock: QualifiedClock) -> Result<Option<Receipt>> {
        let entry = self.current()?;
        // The physical owner remains retained throughout mailbox admission.
        // The guardian independently rechecks its permit and exact pane before
        // any tmux effect; the Bridge's irreversible no-replay boundary is used.
        let physical = lock(&entry.physical)?;
        ensure!(
            !entry.stop.load(Ordering::SeqCst)
                && matches!(physical.outcome, Outcome::Running)
                && physical.stopped.is_none(),
            "native terminal safety stop fenced"
        );
        if let Effect::Input(input) = effect {
            use aq_codex_mapper::InteractiveViewport;
            let view = self.terminal.current_viewport()?;
            match aq_codex_mapper::classify_current_viewport(&view) {
                InteractiveViewport::AuthenticationRequired => {
                    anyhow::bail!("native terminal authentication required; input quarantined");
                }
                InteractiveViewport::TrustDirectory => {
                    let bytes = input.bytes.as_slice();
                    ensure!(
                        bytes == b"\r" || bytes == b"\n" || bytes == b"\x03",
                        "unexpected workspace trust dialog requires explicit controller choice"
                    );
                }
                // V1 direct terminal control permits editing, menus and Ctrl-C
                // while busy; composer readiness gates automated nudges only.
                InteractiveViewport::Ready | InteractiveViewport::Waiting => {}
            }
        }
        self.terminal.submit(effect, clock)
    }
    fn receipt(&self, id: Uuid) -> Result<Option<Receipt>> {
        // Historical metadata must still be publishable after stop.
        self.terminal.receipt(id)
    }
    fn reject_unstarted_input(&self, id: Uuid, digest: &Digest) -> Result<Receipt> {
        self.terminal.reject_unstarted_input(id, digest)
    }
}

pub(super) struct ControlOwner {
    root: PathBuf,
    clock_skew_ms: u64,
    bridge: Mutex<Bridge>,
    physical_job: Arc<Semaphore>,
    // Also covers first registration/attachment, so another attempt's full poll
    // cannot claim input between server registration and the local host join.
    serial: tokio::sync::Mutex<()>,
    http: RelayHttp,
}
impl ControlOwner {
    async fn flush(self: &Arc<Self>) -> Result<()> {
        let pending = self
            .storage(|bridge| {
                bridge.recover_pending()?;
                bridge.prepare_control_upload()
            })
            .await?;
        if let Some(request) = pending {
            let ack = self.http.control_upload(&request).await?;
            self.storage(move |bridge| bridge.acknowledge_control_upload(&ack))
                .await?;
        }
        Ok(())
    }
    async fn storage<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut Bridge) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .physical_job
            .clone()
            .try_acquire_owned()
            .context("native terminal storage busy")?;
        let owner = self.clone();
        tokio::task::spawn_blocking(move || {
            // Cancellation of a waiter cannot release physical admission while
            // its fsync/mailbox effect is still executing.
            let _permit = permit;
            f(&mut lock(&owner.bridge)?)
        })
        .await
        .context("native terminal storage job failed")?
    }
}
#[derive(Clone)]
pub struct PhaseControlRelay {
    owner: Arc<ControlOwner>,
    host: Arc<GuardedTerminal>,
}
impl PhaseControlRelay {
    /// One bounded node-wide exchange. It never holds guardian/journal/Bridge
    /// locks across HTTP. A lost upload repeats its exact persisted metadata.
    pub async fn poll_control_once(&self) -> Result<usize> {
        let _serial = self
            .owner
            .serial
            .try_lock()
            .context("native terminal poll busy")?;
        self.owner.flush().await?;
        self.host.current()?;
        let request = self.owner.storage(|bridge| bridge.poll_request(0)).await?;
        let mut envelope = self.owner.http.poll(&request).await?;
        // Watch has its independent output-only cursor. Capture/epoch are not
        // implemented by this finite control owner and are never acknowledged.
        // Input/Fence/Resize are left exact; a foreign terminal fails closed.
        envelope.effects.retain(|effect| {
            matches!(
                effect,
                Effect::Fence(_) | Effect::Input(_) | Effect::Resize(_)
            )
        });
        let count = self
            .owner
            .storage(move |bridge| {
                let prepared = bridge.receive(&envelope)?;
                let count = prepared.len();
                for effect in prepared {
                    bridge.execute(effect)?;
                }
                bridge.recover_pending()?;
                Ok(count)
            })
            .await?;
        self.owner.flush().await?;
        Ok(count)
    }
    /// Drain already retained receipts even after the host stops. This never
    /// polls/claims new input, creates a replacement host, or replays input.
    pub async fn flush_control_receipts(&self) -> Result<()> {
        let _serial = self.owner.serial.lock().await;
        self.owner.flush().await
    }
    /// Actual current visible plaintext for the harness's local ready/auth
    /// classifier. It is neither a renderer Snapshot nor an input grant.
    pub async fn current_viewport(&self) -> Result<String> {
        let host = self.host.clone();
        self.owner.storage(move |_| host.viewport()).await
    }
}
impl InputPipeline {
    /// Call after actual Started ACK, before starting this host's output
    /// publisher. The caller explicitly supplies its trusted clock-skew bound.
    pub async fn start_phase_control(
        self: &Arc<Self>,
        host: &LocalPhaseHost,
        output: &PhaseOutputRelay,
        clock: QualifiedClock,
    ) -> Result<PhaseControlRelay> {
        ensure!(
            Arc::ptr_eq(&self.owned, &host.owner)
                && Arc::ptr_eq(&self.launch.entry(host.entry.id)?, &host.entry),
            "native control foreign host owner"
        );
        let registration = output.control_registration()?;
        let http = output.control_transport()?;
        let (terminal, root) = {
            let owner = lock(&self.owned)?;
            host.entry.current(&owner)?;
            let a = owner
                .journal
                .state()
                .attempts
                .get(&host.entry.id)
                .context("native control attempt absent")?;
            ensure!(
                a.reservation
                    .offer
                    .offered
                    .offer
                    .manifest
                    .terminal
                    .input_enabled,
                "native control requires original input-enabled profile"
            );
            let history = a
                .native_host
                .as_ref()
                .context("native control history absent")?;
            ensure!(
                registration.terminal == history.intent.terminal
                    && output.terminal() == &registration.terminal,
                "native control terminal/Started mismatch"
            );
            let physical = lock(&host.entry.physical)?;
            ensure!(
                matches!(physical.outcome, Outcome::Running) && physical.stopped.is_none(),
                "native control host is not Running"
            );
            let terminal = physical
                .handle
                .as_ref()
                .context("native control physical handle absent")?
                .bind_terminal(&registration.terminal)?;
            let identity = &owner.journal.state().identity;
            let root = host
                .entry
                .profile
                .as_ref()
                .context("native control profile absent")?
                .root()
                .path()
                .join(format!(
                    "terminal-control-{}-{}",
                    identity.node_id, identity.boot_id
                ));
            (terminal, root)
        };
        let local = Arc::new(GuardedTerminal {
            entry: Arc::downgrade(&host.entry),
            journal: Arc::downgrade(&self.owned),
            output: output.clone(),
            registration: registration.clone(),
            terminal,
        });
        local.current()?;
        let control = {
            let mut saved = lock(&self.launch.control)?;
            if let Some(control) = saved.as_ref() {
                ensure!(
                    control.root == root && control.clock_skew_ms == clock.maximum_skew_ms(),
                    "native control node root or trusted clock bound changed"
                );
                control.clone()
            } else {
                // Explicit reopen retains the old no-replay ledger; corruption
                // is never replaced by an empty/new bridge.
                let bridge = if root.try_exists()? {
                    Bridge::open(&root, registration.peer.clone(), clock)?
                } else {
                    Bridge::create(&root, registration.peer.clone(), clock)?
                };
                let control = Arc::new(ControlOwner {
                    root,
                    clock_skew_ms: clock.maximum_skew_ms(),
                    bridge: Mutex::new(bridge),
                    physical_job: Arc::new(Semaphore::new(1)),
                    serial: tokio::sync::Mutex::new(()),
                    http,
                });
                *saved = Some(control.clone());
                control
            }
        };
        let _serial = control.serial.lock().await;
        let attached = local.clone();
        let exact = registration.clone();
        control
            .storage(move |bridge| {
                bridge.attach(attached)?;
                bridge.prepare_registration_exact(&exact)
            })
            .await?;
        // This is the SAME output/Started-derived Register, verified by the
        // existing HTTP client against original policy and current server owner.
        output.register().await?;
        control
            .storage(move |bridge| bridge.confirm_registration(&registration))
            .await?;
        local.current()?;
        drop(_serial);
        Ok(PhaseControlRelay {
            owner: control,
            host: local,
        })
    }
}
