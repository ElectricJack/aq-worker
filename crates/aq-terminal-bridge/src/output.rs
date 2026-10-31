//! Bounded volatile tail only. It cannot mint snapshots or clear unknown input.
use anyhow::{Result, ensure};
use aq_session_host::terminal_output::ControlEvent;
use aq_session_protocol::{
    Digest, OutputFrame, OutputGap, TerminalIdentity, TerminalPolicy, Validate,
};
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;
pub const MAX_TAIL_FRAMES: usize = 1024;
pub const MAX_TAIL_GAPS: usize = 64;
pub const MAX_NODE_TAILS: usize = 32;
pub const MAX_NODE_TAIL_BYTES: usize = 16 * 1024 * 1024;
/// Volatile node owner: reserves full per-tail byte capacity before attachment,
/// so empty buffers cannot overcommit aggregate policy. Runtime adoption remains
/// separate; standalone OutputTail is a reusable local primitive.
#[derive(Default)]
pub struct OutputTails {
    tails: BTreeMap<Uuid, OutputTail>,
    reserved: usize,
}
impl OutputTails {
    pub fn attach(&mut self, terminal: TerminalIdentity, policy: &TerminalPolicy) -> Result<()> {
        ensure!(
            !self.tails.contains_key(&terminal.terminal_id),
            "output tail already installed; no reset"
        );
        let tail = OutputTail::new(terminal.clone(), policy)?;
        ensure!(
            self.tails.len() < MAX_NODE_TAILS
                && self.reserved + tail.maximum_bytes <= MAX_NODE_TAIL_BYTES,
            "node output reserve exhausted"
        );
        self.reserved += tail.maximum_bytes;
        self.tails.insert(terminal.terminal_id, tail);
        Ok(())
    }
    pub fn tail_mut(&mut self, terminal: &TerminalIdentity) -> Result<&mut OutputTail> {
        let tail = self
            .tails
            .get_mut(&terminal.terminal_id)
            .ok_or_else(|| anyhow::anyhow!("output tail absent"))?;
        ensure!(
            tail.terminal == *terminal,
            "output tail incarnation mismatch"
        );
        Ok(tail)
    }
    pub fn reserved_bytes(&self) -> usize {
        self.reserved
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailState {
    InitialIncomplete,
    Discontinuous,
}
pub struct OutputTail {
    terminal: TerminalIdentity,
    maximum_frame: usize,
    maximum_bytes: usize,
    bytes: usize,
    next: u64,
    frames: VecDeque<OutputFrame>,
    gaps: VecDeque<OutputGap>,
    state: TailState,
}
impl OutputTail {
    pub fn new(terminal: TerminalIdentity, policy: &TerminalPolicy) -> Result<Self> {
        terminal.validate()?;
        ensure!(
            policy.max_frame_bytes > 0
                && policy.max_frame_bytes as usize <= aq_session_protocol::MAX_FRAME_BYTES
                && policy.replay_bytes >= policy.max_frame_bytes
                && policy.replay_bytes as usize <= aq_session_protocol::MAX_REPLAY_BYTES,
            "output tail sealed policy bounds"
        );
        Ok(Self {
            terminal,
            maximum_frame: policy.max_frame_bytes as usize,
            maximum_bytes: policy.replay_bytes as usize,
            bytes: 0,
            next: 1,
            frames: VecDeque::new(),
            gaps: VecDeque::new(),
            state: TailState::InitialIncomplete,
        })
    }
    pub fn state(&self) -> TailState {
        self.state
    }
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }
    pub fn next_sequence(&self) -> u64 {
        self.next
    }
    pub fn frames(&self) -> impl Iterator<Item = &OutputFrame> {
        self.frames.iter()
    }
    pub fn gaps(&self) -> impl Iterator<Item = &OutputGap> {
        self.gaps.iter()
    }
    pub fn accept(&mut self, event: &ControlEvent) -> Result<()> {
        ensure!(
            self.state != TailState::Discontinuous,
            "tail producer discontinuous; no implicit restart"
        );
        match event {
            ControlEvent::Bytes(bytes) => {
                ensure!(
                    bytes.len() <= aq_session_host::terminal_output::MAX_CONTROL_READ,
                    "output tail input byte bound"
                );
                for chunk in bytes.chunks(self.maximum_frame) {
                    if chunk.is_empty() {
                        continue;
                    }
                    let sequence = self.next;
                    self.next = self
                        .next
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("output sequence exhausted"))?;
                    let frame = OutputFrame {
                        terminal: self.terminal.clone(),
                        sequence,
                        bytes: chunk.to_vec(),
                        byte_length: chunk.len() as u32,
                        digest: Digest::of(chunk),
                    };
                    frame.validate()?;
                    self.bytes += frame.bytes.len();
                    self.frames.push_back(frame);
                    while self.bytes > self.maximum_bytes || self.frames.len() > MAX_TAIL_FRAMES {
                        let old = self.frames.pop_front().unwrap();
                        self.bytes -= old.bytes.len();
                        self.add_gap(old.sequence)?;
                    }
                }
            }
            ControlEvent::Paused
            | ControlEvent::Continued
            | ControlEvent::Exited
            | ControlEvent::CommandError => {
                // Unknown missing bytes have no invented exact sequence range.
                self.state = TailState::Discontinuous;
            }
            ControlEvent::LayoutChanged | ControlEvent::Attached => {} // no renderer/dimension-state claim
        }
        Ok(())
    }
    fn add_gap(&mut self, sequence: u64) -> Result<()> {
        if let Some(last) = self.gaps.back_mut()
            && last.last_missing.checked_add(1) == Some(sequence)
        {
            last.last_missing = sequence;
            return Ok(());
        }
        ensure!(
            self.gaps.len() < MAX_TAIL_GAPS,
            "output gap metadata capacity"
        );
        self.gaps.push_back(OutputGap {
            terminal: self.terminal.clone(),
            first_missing: sequence,
            last_missing: sequence,
        });
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use aq_session_protocol::SessionIdentity;
    use uuid::Uuid;
    fn tail() -> OutputTail {
        let terminal = TerminalIdentity {
            session: SessionIdentity::Central {
                authority_id: Uuid::from_u128(1),
                session_id: Uuid::from_u128(2),
                generation: 1,
            },
            terminal_id: Uuid::from_u128(3),
            epoch: 1,
        };
        OutputTail::new(
            terminal,
            &TerminalPolicy {
                host: aq_session_protocol::TerminalHost::Tmux,
                input_enabled: false,
                max_input_bytes: 1,
                max_pending_input_bytes: 1,
                max_frame_bytes: 4,
                replay_bytes: 8,
            },
        )
        .unwrap()
    }
    #[test]
    fn exact_eviction_ranges_keep_physical_epoch_and_monotonic_sequence() {
        let mut tail = tail();
        tail.accept(&ControlEvent::Bytes(b"abcdefghijkl".to_vec()))
            .unwrap();
        assert_eq!(tail.retained_bytes(), 8);
        assert_eq!(tail.next_sequence(), 4);
        let frames: Vec<_> = tail.frames().collect();
        assert_eq!(frames[0].sequence, 2);
        assert_eq!(frames[1].sequence, 3);
        assert!(frames.iter().all(|f| f.terminal.epoch == 1));
        assert_eq!(tail.gaps().next().unwrap().first_missing, 1);
        tail.accept(&ControlEvent::Bytes(b"mnop".to_vec())).unwrap();
        assert_eq!(tail.gaps().next().unwrap().last_missing, 2);
        assert_eq!(tail.state(), TailState::InitialIncomplete);
    }
    #[test]
    fn pause_and_exit_never_fabricate_gap_extent_snapshot_or_restart() {
        let mut tail = tail();
        tail.accept(&ControlEvent::Bytes(vec![0xe9, 0x9b])).unwrap();
        tail.accept(&ControlEvent::Paused).unwrap();
        assert_eq!(tail.state(), TailState::Discontinuous);
        assert_eq!(tail.gaps().count(), 0);
        assert_eq!(tail.next_sequence(), 2);
        assert!(tail.accept(&ControlEvent::Bytes(vec![0xaa])).is_err());
    }
    #[test]
    fn node_reserves_policy_capacity_and_never_rebinds_a_terminal_id() {
        let mut owner = OutputTails::default();
        let template = tail();
        let policy = TerminalPolicy {
            host: aq_session_protocol::TerminalHost::Tmux,
            input_enabled: false,
            max_input_bytes: 1,
            max_pending_input_bytes: 1,
            max_frame_bytes: 4096,
            replay_bytes: 1048576,
        };
        for n in 1..=16 {
            let mut terminal = template.terminal.clone();
            terminal.terminal_id = Uuid::from_u128(n);
            owner.attach(terminal, &policy).unwrap();
        }
        assert_eq!(owner.reserved_bytes(), MAX_NODE_TAIL_BYTES);
        let mut terminal = template.terminal.clone();
        terminal.terminal_id = Uuid::from_u128(17);
        assert!(owner.attach(terminal, &policy).is_err());
        let mut old = template.terminal.clone();
        old.terminal_id = Uuid::from_u128(1);
        old.epoch += 1;
        assert!(owner.tail_mut(&old).is_err());
        assert!(owner.attach(old, &policy).is_err());
    }
}
