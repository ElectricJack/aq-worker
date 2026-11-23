//! One local sequencer owns renderer updates, replay eviction and snapshot cuts.
//! Only the verified pre-output host baseline admits a complete physical image.
use anyhow::{Context, Result, ensure};
use aq_session_host::{observed::VerifiedBlankBaseline, terminal_output::ControlEvent};
use aq_session_protocol::{
    ArtifactRef, Digest, OutputFrame, OutputGap, Snapshot, TerminalIdentity, TerminalPolicy,
    Validate,
};
use aq_terminal_bridge::output::{OutputTail, TailState};
use aq_terminal_relay_protocol::{self as wire, Peer, PeerUpload};
use aq_terminal_renderer::{AssumedBlank, OrderedResize, Renderer};
use std::collections::BTreeMap;
use uuid::Uuid;
mod upload;

pub const MAX_NODE_REPLAY: usize = 32 * 1024 * 1024;
pub const MAX_NODE_SUBSCRIPTIONS: usize = 32;
pub const MAX_SESSION_VIEWERS: usize = 4;
pub const MAX_TAIL_UPLOAD: usize = 256 * 1024;
pub const MAX_IMAGE_UPLOAD: usize = 1024 * 1024;
pub const SESSION_BYTES_PER_SECOND: usize = 256 * 1024;
pub const NODE_BYTES_PER_SECOND: usize = 2 * 1024 * 1024;

#[derive(Clone)]
struct Bucket {
    capacity: usize,
    tokens: usize,
    last: std::time::Instant,
    remainder: u128,
}
impl Bucket {
    fn new(capacity: usize, now: std::time::Instant) -> Self {
        Self {
            capacity,
            tokens: capacity,
            last: now,
            remainder: 0,
        }
    }
    fn take(&mut self, bytes: usize, now: std::time::Instant) -> bool {
        let Some(elapsed) = now.checked_duration_since(self.last) else {
            return false;
        };
        let minted = elapsed.as_nanos().saturating_mul(self.capacity as u128) + self.remainder;
        self.tokens = self.capacity.min(
            self.tokens
                .saturating_add((minted / 1_000_000_000).min(usize::MAX as u128) as usize),
        );
        self.remainder = minted % 1_000_000_000;
        self.last = now;
        if bytes > self.tokens {
            return false;
        }
        self.tokens -= bytes;
        true
    }
}
pub(crate) struct OutputRate(Bucket);
impl Default for OutputRate {
    fn default() -> Self {
        Self(Bucket::new(
            SESSION_BYTES_PER_SECOND,
            std::time::Instant::now(),
        ))
    }
}
impl OutputRate {
    /// One process-wide node bucket, in addition to the authoritative server's
    /// node aggregate across separate sandbox processes. Never sleeps or waits
    /// for output capacity while holding an execution/renewal lock.
    pub fn admit(&mut self, bytes: usize) -> Result<bool> {
        use std::sync::{Mutex, OnceLock};
        static NODE: OnceLock<Mutex<Bucket>> = OnceLock::new();
        let now = std::time::Instant::now();
        let mut node = NODE
            .get_or_init(|| Mutex::new(Bucket::new(NODE_BYTES_PER_SECOND, now)))
            .lock()
            .map_err(|_| anyhow::anyhow!("node output budget poisoned"))?;
        let mut session = self.0.clone();
        let mut aggregate = node.clone();
        if !session.take(bytes, now) || !aggregate.take(bytes, now) {
            return Ok(false);
        }
        self.0 = session;
        *node = aggregate;
        Ok(true)
    }
}

/// Immutable single in-flight exchange. Retry this request before taking
/// another cut; new observer bytes go only into bounded replay and the renderer.
pub(crate) struct OutputBatch {
    pub request: PeerUpload,
    pub digest: Digest,
    pub through: u64,
    pub payload_bytes: usize,
}

pub struct TerminalOutput {
    terminal: TerminalIdentity,
    tail: OutputTail,
    renderer: Option<Renderer>,
    artifact: Option<ArtifactRef>,
    maximum_frame: usize,
    renderer_gap: Option<OutputGap>,
}
impl TerminalOutput {
    pub fn from_verified(
        baseline: &VerifiedBlankBaseline,
        policy: &TerminalPolicy,
    ) -> Result<Self> {
        Self::from_blank(
            baseline.terminal().clone(),
            policy,
            baseline.renderer().clone(),
            baseline.dimensions(),
        )
    }
    // The native runner's retained baseline is joined to its live host before
    // entering here. This constructor is never exposed to network callers.
    pub(crate) fn from_blank(
        terminal: TerminalIdentity,
        policy: &TerminalPolicy,
        artifact: ArtifactRef,
        (columns, rows): (u16, u16),
    ) -> Result<Self> {
        let mut output = Self::tail_only(terminal.clone(), policy)?;
        output.renderer = Some(Renderer::from_assumed_blank(AssumedBlank {
            terminal,
            columns,
            rows,
        })?);
        output.artifact = Some(artifact);
        Ok(output)
    }
    pub(crate) fn tail_only(terminal: TerminalIdentity, policy: &TerminalPolicy) -> Result<Self> {
        Ok(Self {
            tail: OutputTail::new(terminal.clone(), policy)?,
            terminal,
            renderer: None,
            artifact: None,
            maximum_frame: (policy.max_frame_bytes as usize).min((MAX_TAIL_UPLOAD - 16 * 1024) / 4),
            renderer_gap: None,
        })
    }
    pub fn tail(&self) -> &Self {
        self
    }
    pub fn terminal(&self) -> &TerminalIdentity {
        &self.terminal
    }
    pub fn frames(&self) -> impl Iterator<Item = &OutputFrame> {
        self.tail.frames()
    }
    pub fn gaps(&self) -> impl Iterator<Item = &OutputGap> {
        self.tail.gaps().chain(self.renderer_gap.iter())
    }
    pub fn next_sequence(&self) -> u64 {
        self.tail.next_sequence()
    }
    pub fn retained_bytes(&self) -> usize {
        self.tail.retained_bytes()
    }
    pub fn state(&self) -> TailState {
        self.tail.state()
    }
    pub fn accept(&mut self, event: &ControlEvent) -> Result<()> {
        let result = self.accept_inner(event);
        if result.is_err() {
            self.invalidate();
            let _ = self.tail.accept(&ControlEvent::CommandError);
        }
        result
    }
    fn accept_inner(&mut self, event: &ControlEvent) -> Result<()> {
        match event {
            ControlEvent::Bytes(bytes) => {
                ensure!(
                    bytes.len() <= aq_session_host::terminal_output::MAX_CONTROL_READ,
                    "output input byte bound"
                );
                // Feed each frame before the next append can evict it. Renderer
                // state remains complete even if a slow viewer loses replay.
                for chunk in bytes.chunks(self.maximum_frame) {
                    self.append_chunk(chunk)?;
                }
            }
            ControlEvent::LayoutChanged => {
                // A notification has no trusted ordering/dimensions. Only the
                // explicit ordered resize API can preserve consistency.
                self.invalidate();
            }
            ControlEvent::Paused
            | ControlEvent::Continued
            | ControlEvent::Exited
            | ControlEvent::CommandError => {
                self.invalidate();
                self.tail.accept(event)?;
            }
            ControlEvent::Attached => self.tail.accept(event)?,
        }
        Ok(())
    }
    fn append_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.tail.accept(&ControlEvent::Bytes(chunk.to_vec()))?;
        let frame = self.tail.frames().last().context("output frame absent")?;
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        let missing = || OutputGap {
            terminal: frame.terminal.clone(),
            first_missing: frame.sequence,
            last_missing: frame.sequence,
        };
        if !renderer.is_consistent() {
            self.renderer_gap.get_or_insert_with(missing).last_missing = frame.sequence;
        } else if renderer
            .bytes(renderer.through_ordinal() + 1, frame)
            .is_err()
        {
            self.renderer_gap = Some(missing());
        }
        Ok(())
    }
    pub fn invalidate(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            renderer.gap();
        }
    }
    pub fn resize(&mut self, after_output_sequence: u64, columns: u16, rows: u16) -> Result<()> {
        let renderer = self
            .renderer
            .as_mut()
            .context("output has no verified baseline")?;
        renderer.resize(OrderedResize {
            ordinal: renderer.through_ordinal() + 1,
            after_output_sequence,
            columns,
            rows,
        })?;
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let renderer = self
            .renderer
            .as_ref()
            .context("output has no verified baseline")?;
        let image = renderer.image()?;
        let screen = renderer.screen();
        let snapshot = Snapshot {
            snapshot_id: Uuid::new_v4(),
            capture_request_id: Uuid::new_v4(),
            terminal: renderer.terminal().clone(),
            through_sequence: renderer.through_sequence(),
            columns: screen.columns as u16,
            rows: screen.rows as u16,
            renderer: self
                .artifact
                .clone()
                .context("output renderer pin absent")?,
            state: image.bytes().to_vec(),
            digest: image.digest().clone(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn restore(snapshot: &Snapshot) -> Result<Renderer> {
        snapshot.validate()?;
        let renderer = Renderer::restore(&snapshot.terminal, &snapshot.digest, &snapshot.state)?;
        ensure!(
            renderer.through_sequence() == snapshot.through_sequence
                && renderer.screen().columns == snapshot.columns as usize
                && renderer.screen().rows == snapshot.rows as usize,
            "snapshot watermark or geometry mismatch"
        );
        Ok(renderer)
    }
}

/// Reserve full replay capacity before attaching an observer; empty sessions
/// cannot overcommit the node. This is also the bounded output upload queue.
#[derive(Default)]
pub(crate) struct OutputStreams {
    streams: BTreeMap<Uuid, TerminalOutput>,
    reserved: usize,
}
impl OutputStreams {
    #[cfg(test)]
    pub fn attach(&mut self, terminal: TerminalIdentity, policy: &TerminalPolicy) -> Result<()> {
        self.insert(terminal, policy, None)
    }
    pub fn attach_blank(
        &mut self,
        baseline: AssumedBlank,
        policy: &TerminalPolicy,
        artifact: ArtifactRef,
    ) -> Result<()> {
        let terminal = baseline.terminal;
        let output = TerminalOutput::from_blank(
            terminal.clone(),
            policy,
            artifact,
            (baseline.columns, baseline.rows),
        )?;
        self.insert(terminal, policy, Some(output))
    }
    fn insert(
        &mut self,
        terminal: TerminalIdentity,
        policy: &TerminalPolicy,
        output: Option<TerminalOutput>,
    ) -> Result<()> {
        ensure!(
            !self.streams.contains_key(&terminal.terminal_id),
            "output already installed; no epoch reuse"
        );
        ensure!(
            self.streams.len() < MAX_NODE_SUBSCRIPTIONS
                && self.reserved + policy.replay_bytes as usize <= MAX_NODE_REPLAY,
            "node output reserve exhausted"
        );
        let output = match output {
            Some(output) => output,
            None => TerminalOutput::tail_only(terminal.clone(), policy)?,
        };
        self.reserved += policy.replay_bytes as usize;
        self.streams.insert(terminal.terminal_id, output);
        Ok(())
    }
    pub fn tail_mut(&mut self, terminal: &TerminalIdentity) -> Result<&mut TerminalOutput> {
        let output = self
            .streams
            .get_mut(&terminal.terminal_id)
            .context("output absent")?;
        ensure!(output.terminal == *terminal, "output epoch mismatch");
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
