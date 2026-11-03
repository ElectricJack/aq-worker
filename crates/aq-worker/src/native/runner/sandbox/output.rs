//! Bounded volatile terminal lane. Only its own upload lock crosses HTTP awaits.
use super::{
    auth::{RunnerHttp, RunnerRoute, TerminalRoute},
    channels::TerminalDemand,
};
use crate::native::terminal_output::{MAX_IMAGE_UPLOAD, OutputBatch, OutputRate, TerminalOutput};
use anyhow::{Context, Result, ensure};
use aq_session_host::terminal_output::{ControlEvent, OutputObserver};
use aq_session_protocol::Digest;
use aq_terminal_relay_protocol::{Peer, UploadAck};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex as AsyncMutex, watch};

pub struct SandboxOutput {
    http: RunnerHttp,
    peer: Peer,
    output: Mutex<TerminalOutput>,
    upload: AsyncMutex<Upload>,
    fresh: AtomicBool,
    demand: watch::Receiver<TerminalDemand>,
}
#[derive(Default)]
struct Upload {
    through: u64,
    pending: Option<OutputBatch>,
    rate: OutputRate,
}
impl SandboxOutput {
    pub(crate) fn new(
        http: &RunnerHttp,
        output: TerminalOutput,
        demand: watch::Receiver<TerminalDemand>,
    ) -> Result<Self> {
        ensure!(
            output.terminal().session == http.launch_binding().session,
            "output belongs to another sandbox"
        );
        let binding = http.binding();
        http.check_scope(RunnerRoute::Terminal(TerminalRoute::OutputUpload), &binding)?;
        Ok(Self {
            http: http.channel_pool()?,
            peer: Peer::Remote {
                node_id: binding.node.node_id,
                boot_id: binding.node.boot_id,
            },
            output: Mutex::new(output),
            upload: AsyncMutex::new(Upload::default()),
            fresh: AtomicBool::new(true),
            demand,
        })
    }
    pub(crate) fn terminal(&self) -> Result<aq_session_protocol::TerminalIdentity> {
        Ok(self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("output queue poisoned"))?
            .terminal()
            .clone())
    }
    pub fn accept(&self, event: &ControlEvent) -> Result<()> {
        self.output
            .lock()
            .map_err(|_| anyhow::anyhow!("output queue poisoned"))?
            .accept(event)
    }
    pub fn retained_bytes(&self) -> Result<usize> {
        Ok(self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("output queue poisoned"))?
            .retained_bytes())
    }
    pub fn has_gap(&self) -> Result<bool> {
        Ok(self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("output queue poisoned"))?
            .gaps()
            .next()
            .is_some())
    }
    /// A reconnect requests a cut; it never reopens or relaunches the host.
    pub fn request_snapshot(&self) {
        self.fresh.store(true, Ordering::SeqCst);
    }
    pub async fn upload_once(&self) -> Result<Option<UploadAck>> {
        if self.demand.borrow().subscriptions == 0 {
            return Ok(None);
        }
        let mut upload = self
            .upload
            .try_lock()
            .context("sandbox output upload busy")?;
        if upload.pending.is_none() {
            let output = self
                .output
                .lock()
                .map_err(|_| anyhow::anyhow!("output queue poisoned"))?;
            upload.pending = output.prepare(
                &self.peer,
                upload.through,
                self.fresh.swap(false, Ordering::SeqCst),
            )?;
        }
        let Some(pending) = upload.pending.as_ref() else {
            return Ok(None);
        };
        let charge = pending.payload_bytes;
        if !upload.rate.admit(charge)? {
            return Ok(None);
        }
        let pending = upload.pending.as_ref().unwrap();
        ensure!(
            Digest::of(&serde_json::to_vec(&pending.request)?) == pending.digest,
            "output retry changed"
        );
        let bytes = self
            .http
            .post_bounded(
                RunnerRoute::Terminal(TerminalRoute::OutputUpload),
                &self.http.binding(),
                &pending.request,
                MAX_IMAGE_UPLOAD,
                64 * 1024,
            )
            .await?;
        let ack: UploadAck = serde_json::from_slice(&bytes)?;
        ack.validate()?;
        ensure!(
            ack.peer == self.peer
                && ack.exchange_id == pending.request.exchange_id
                && ack.cursor == pending.request.cursor
                && ack.capability == pending.request.capability
                && ack.retirements.is_empty(),
            "output acknowledgement binding mismatch"
        );
        ensure!(
            ack.dropped_output.iter().all(|gap| {
                gap.last_missing
                    .checked_sub(gap.first_missing)
                    .and_then(|n| n.checked_add(1))
                    .is_some_and(|count| {
                        count <= 64
                            && pending
                                .request
                                .frames
                                .iter()
                                .filter(|f| {
                                    f.terminal == gap.terminal
                                        && f.sequence >= gap.first_missing
                                        && f.sequence <= gap.last_missing
                                })
                                .count() as u64
                                == count
                    })
            }),
            "foreign output drop range"
        );
        upload.through = pending.through;
        upload.pending = None;
        Ok(Some(ack))
    }
    /// Upload runs independently from the observer and execution loop. Dropping
    /// a held HTTP future retains the exact pending exchange in this owner.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(50));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            tokio::select! { biased;
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } }
                _ = timer.tick() => {
                    tokio::select! { biased;
                        changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } }
                        result = self.upload_once() => { if self.http.authority_lost() { return result.map(|_| ()); } }
                    }
                }
            }
        }
    }
    /// Consume the already opened pre-release observer. A reconnect cannot call
    /// this with another observer; construction of that lease belongs to host.
    pub async fn drain(
        self: Arc<Self>,
        mut observer: OutputObserver,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        ensure!(
            observer.terminal()
                == self
                    .output
                    .lock()
                    .map_err(|_| anyhow::anyhow!("output queue poisoned"))?
                    .terminal(),
            "observer output identity mismatch"
        );
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(10));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            tokio::select! { biased;
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } }
                _ = timer.tick() => {
                    match observer.poll() {
                        Ok(events) => for event in events { self.accept(&event)?; },
                        Err(error) => { self.accept(&ControlEvent::CommandError)?; return Err(error); }
                    }
                }
            }
        }
    }
}
