//! Build one bounded, ordered replay prefix for the peer.
use super::*;
struct Prefix {
    request: PeerUpload,
    through: u64,
    payload_bytes: usize,
    limit: usize,
}
impl Prefix {
    fn finish(self) -> Result<Option<OutputBatch>> {
        let Prefix {
            request,
            through,
            payload_bytes,
            limit,
        } = self;
        let empty =
            request.frames.is_empty() && request.gaps.is_empty() && request.snapshots.is_empty();
        if empty {
            return Ok(None);
        }
        request.validate()?;
        let bytes = serde_json::to_vec(&request)?;
        ensure!(bytes.len() <= limit, "output upload byte bound");
        Ok(Some(OutputBatch {
            request,
            digest: Digest::of(&bytes),
            through,
            payload_bytes,
        }))
    }
}
impl TerminalOutput {
    pub(crate) fn prepare(
        &self,
        peer: &Peer,
        through: u64,
        fresh: bool,
    ) -> Result<Option<OutputBatch>> {
        let mut request = PeerUpload {
            version: wire::VERSION,
            capability: aq_session_protocol::Text::new(wire::CAPABILITY)?,
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
        let mut barrier = through;
        let mut payload_bytes = 0;
        let needs_snapshot = fresh || self.gaps().any(|gap| gap.last_missing > through);
        if needs_snapshot && let Ok(snapshot) = self.snapshot() {
            ensure!(snapshot.through_sequence >= through, "snapshot regressed");
            barrier = snapshot.through_sequence;
            payload_bytes = snapshot.state.len();
            request.snapshots.push(snapshot);
        }
        let limit = if request.snapshots.is_empty() {
            MAX_TAIL_UPLOAD
        } else {
            MAX_IMAGE_UPLOAD
        };
        let mut prefix = Prefix {
            request,
            through: barrier,
            payload_bytes,
            limit,
        };
        self.append_leading_gaps(&mut prefix)?;
        self.append_frames(&mut prefix)?;
        prefix.finish()
    }
    fn append_leading_gaps(&self, prefix: &mut Prefix) -> Result<()> {
        let barrier = prefix.through;
        // The peer applies every gap before every frame. Send only the next
        // ordered prefix: leading gaps, followed by frames up to the next gap.
        // In particular a renderer inconsistency can overlap retained raw bytes;
        // those bytes cannot be replayed after declaring their sequence missing.
        let first_frame = self
            .frames()
            .find(|frame| {
                frame.sequence > barrier
                    && !self.gaps().any(|gap| {
                        gap.first_missing <= frame.sequence && frame.sequence <= gap.last_missing
                    })
            })
            .map(|frame| frame.sequence);
        for gap in self
            .gaps()
            .filter(|g| {
                g.last_missing > barrier && first_frame.is_none_or(|first| g.last_missing < first)
            })
            .take(64)
        {
            let mut gap = gap.clone();
            gap.first_missing = gap.first_missing.max(barrier + 1);
            prefix.request.gaps.push(gap.clone());
            if serde_json::to_vec(&prefix.request)?.len() > prefix.limit {
                prefix.request.gaps.pop();
                break;
            }
            prefix.through = prefix.through.max(gap.last_missing);
        }
        Ok(())
    }
    fn append_frames(&self, prefix: &mut Prefix) -> Result<()> {
        let frame_barrier = prefix.through;
        let next_gap = self
            .gaps()
            .filter(|g| g.last_missing > frame_barrier)
            .map(|g| g.first_missing.max(frame_barrier + 1))
            .min()
            .unwrap_or(u64::MAX);
        for frame in self
            .frames()
            .filter(|f| f.sequence > frame_barrier && f.sequence < next_gap)
            .take(64)
        {
            if prefix.payload_bytes + frame.bytes.len() > SESSION_BYTES_PER_SECOND {
                break;
            }
            prefix.request.frames.push(frame.clone());
            if serde_json::to_vec(&prefix.request)?.len() > prefix.limit {
                prefix.request.frames.pop();
                break;
            }
            prefix.payload_bytes += frame.bytes.len();
            prefix.through = prefix.through.max(frame.sequence);
        }
        Ok(())
    }
}
