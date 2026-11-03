use super::*;
use aq_session_host::terminal_output::ControlEvent;
use aq_session_protocol::{
    ArtifactRef, Digest, SessionIdentity, Snapshot, TerminalHost, TerminalIdentity, TerminalPolicy,
    Text, Validate,
};
use aq_terminal_renderer::Renderer;
use uuid::Uuid;

fn terminal() -> TerminalIdentity {
    TerminalIdentity {
        session: SessionIdentity::Central {
            authority_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(2),
            generation: 1,
        },
        terminal_id: Uuid::from_u128(3),
        epoch: 1,
    }
}
fn policy() -> TerminalPolicy {
    TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: false,
        max_input_bytes: 1,
        max_pending_input_bytes: 1,
        max_frame_bytes: 4,
        replay_bytes: 8,
    }
}
fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("aq-terminal-renderer").unwrap(),
        version: Text::new("0.1.0").unwrap(),
        digest: Digest::of(b"pinned-test-renderer"),
    }
}
fn fixture() -> TerminalOutput {
    TerminalOutput::from_blank(terminal(), &policy(), artifact(), 40, 4).unwrap()
}
fn text(renderer: &Renderer) -> String {
    renderer
        .screen()
        .line(0)
        .unwrap()
        .iter()
        .map(|c| c.glyph())
        .collect()
}

// Omitting pre-eviction renderer ingestion or replaying a covered frame corrupts
// the literal expected row; restoring a partial parser is also required.
#[test]
fn reconnect_after_gap_renders_from_snapshot() {
    let mut output = fixture();
    for bytes in [b"hello ".as_slice(), b"world ", b"\xe9\x9b"] {
        output.accept(&ControlEvent::Bytes(bytes.to_vec())).unwrap();
    }
    assert!(output.tail().gaps().next().is_some());
    let cut = output.snapshot().unwrap();
    assert_eq!(cut.through_sequence, 5);
    let encoded = serde_json::to_vec(&cut).unwrap();
    let cut: Snapshot = serde_json::from_slice(&encoded).unwrap();
    cut.validate().unwrap();
    // Output continues while the immutable cut is in a blocked upload.
    for bytes in [b"\xaa\x1b[3".as_slice(), b"1m!\x1b[0m"] {
        output.accept(&ControlEvent::Bytes(bytes.to_vec())).unwrap();
    }
    let mut viewer = Renderer::restore(&cut.terminal, &cut.digest, &cut.state).unwrap();
    assert_eq!(viewer.through_sequence(), cut.through_sequence);
    // A small replay buffer has evicted the post-cut prefix: reconnect needs a
    // fresh cut rather than pretending the original cut can bridge that gap.
    assert!(
        output
            .tail()
            .gaps()
            .any(|g| g.last_missing > cut.through_sequence)
    );
    let fresh = output.snapshot().unwrap();
    viewer = Renderer::restore(&fresh.terminal, &fresh.digest, &fresh.state).unwrap();
    output.accept(&ControlEvent::Bytes(b"ok".to_vec())).unwrap();
    for frame in output
        .tail()
        .frames()
        .filter(|f| f.sequence > fresh.through_sequence)
    {
        viewer.bytes(viewer.through_ordinal() + 1, frame).unwrap();
    }
    assert_eq!(text(&viewer), "hello world 雪!ok");
    assert_eq!(viewer.through_sequence(), output.tail().next_sequence() - 1);
}

#[test]
fn ordered_resize_and_unsupported_or_missing_output_do_not_guess() {
    let mut output = fixture();
    output
        .accept(&ControlEvent::Bytes(b"abc".to_vec()))
        .unwrap();
    output.resize(1, 20, 3).unwrap();
    let snapshot = output.snapshot().unwrap();
    assert_eq!((snapshot.columns, snapshot.rows), (20, 3));
    let restored =
        Renderer::restore(&snapshot.terminal, &snapshot.digest, &snapshot.state).unwrap();
    assert_eq!(text(&restored), "abc");
    output
        .accept(&ControlEvent::Bytes(b"\x1bPunsupported\x1b\\".to_vec()))
        .unwrap();
    assert!(output.snapshot().is_err());
    assert!(output.tail().gaps().next().is_some());
    let mut changed = fixture();
    changed.accept(&ControlEvent::LayoutChanged).unwrap();
    assert!(changed.snapshot().is_err());
    let mut lost = fixture();
    lost.accept(&ControlEvent::Paused).unwrap();
    assert!(lost.snapshot().is_err());
    let mut oversized = fixture();
    assert!(
        oversized
            .accept(&ControlEvent::Bytes(vec![b'x'; 65537]))
            .is_err()
    );
    assert!(oversized.snapshot().is_err());
}

#[test]
fn snapshot_restore_checks_epoch_watermark_and_digest() {
    let mut output = fixture();
    output
        .accept(&ControlEvent::Bytes(b"abc".to_vec()))
        .unwrap();
    let mut snapshot = output.snapshot().unwrap();
    assert!(TerminalOutput::restore(&snapshot).is_ok());
    snapshot.through_sequence += 1;
    assert!(TerminalOutput::restore(&snapshot).is_err());
    snapshot.through_sequence -= 1;
    snapshot.terminal.epoch += 1;
    assert!(TerminalOutput::restore(&snapshot).is_err());
    let fresh = TerminalOutput::from_blank(snapshot.terminal.clone(), &policy(), artifact(), 40, 4)
        .unwrap();
    assert_eq!(fresh.snapshot().unwrap().through_sequence, 0);
    assert_eq!(
        text(&TerminalOutput::restore(&fresh.snapshot().unwrap()).unwrap()),
        ""
    );
    snapshot.state[0] ^= 1;
    assert!(TerminalOutput::restore(&snapshot).is_err());
}

#[test]
fn snapshot_upload_cut_survives_buffering_and_retransmit() {
    use aq_terminal_relay::draft::Peer;
    let mut output = fixture();
    output
        .accept(&ControlEvent::Bytes(b"before".to_vec()))
        .unwrap();
    let peer = aq_terminal_relay_protocol::Peer::Central {
        authority_id: Uuid::from_u128(1),
    };
    let pending = output.prepare(&peer, 0, true).unwrap().unwrap();
    let encoded = serde_json::to_vec(&pending.request).unwrap();
    let request: aq_terminal_relay_protocol::PeerUpload = serde_json::from_slice(&encoded).unwrap();
    let snapshot = &request.snapshots[0];
    assert_eq!(snapshot.through_sequence, 2);
    assert!(request.frames.is_empty());
    let mut replay = aq_terminal_relay::output::Replay::new(terminal(), 1024 * 1024).unwrap();
    let origin = Peer::Central {
        authority_id: Uuid::from_u128(1),
    };
    assert!(replay.snapshot(&origin, snapshot.clone(), 1).unwrap());
    assert!(!replay.snapshot(&origin, snapshot.clone(), 1).unwrap());
    output
        .accept(&ControlEvent::Bytes(b"after".to_vec()))
        .unwrap();
    assert_eq!(serde_json::to_vec(&pending.request).unwrap(), encoded);
    let later = output
        .prepare(&peer, pending.through, false)
        .unwrap()
        .unwrap();
    assert!(later.request.snapshots.is_empty());
    let mut viewer = TerminalOutput::restore(snapshot).unwrap();
    for frame in later.request.frames {
        assert!(frame.sequence > snapshot.through_sequence);
        assert!(replay.frame(&origin, frame.clone(), 2).unwrap());
        assert!(!replay.frame(&origin, frame.clone(), 2).unwrap());
        let mut changed = frame.clone();
        changed.bytes[0] ^= 1;
        changed.digest = Digest::of(&changed.bytes);
        assert!(replay.frame(&origin, changed, 2).is_err());
        viewer.bytes(viewer.through_ordinal() + 1, &frame).unwrap();
    }
    assert_eq!(text(&viewer), "beforeafter");
}

#[test]
fn h7_review_blank_ack_is_idle_until_reconnect() {
    let output = fixture();
    let peer = aq_terminal_relay_protocol::Peer::Central {
        authority_id: Uuid::from_u128(1),
    };
    let first = output.prepare(&peer, 0, true).unwrap().unwrap();
    assert_eq!(first.through, 0);
    assert_eq!(first.request.snapshots.len(), 1);
    assert!(
        output
            .prepare(&peer, first.through, false)
            .unwrap()
            .is_none()
    );
    let reconnect = output.prepare(&peer, first.through, true).unwrap().unwrap();
    assert_eq!(reconnect.request.snapshots.len(), 1);
    assert_eq!(reconnect.through, 0);
}

#[test]
fn h7_review_inconsistent_batches_apply_in_server_order() {
    let mut output = fixture();
    output
        .accept(&ControlEvent::Bytes(b"good".to_vec()))
        .unwrap();
    output
        .accept(&ControlEvent::Bytes(b"\x1bPbad".to_vec()))
        .unwrap();
    output
        .accept(&ControlEvent::Bytes(b"tail".to_vec()))
        .unwrap();
    assert!(output.snapshot().is_err());
    let peer = aq_terminal_relay_protocol::Peer::Central {
        authority_id: Uuid::from_u128(1),
    };
    let origin = aq_terminal_relay::draft::Peer::Central {
        authority_id: Uuid::from_u128(1),
    };
    let mut replay = aq_terminal_relay::output::Replay::new(terminal(), 1024 * 1024).unwrap();
    let mut through = 0;
    let mut sent = Vec::new();
    let mut gaps = Vec::new();
    for _ in 0..4 {
        let Some(batch) = output.prepare(&peer, through, false).unwrap() else {
            break;
        };
        assert!(batch.through > through);
        // The actual peer applies gaps, then snapshots, then frames.
        for gap in &batch.request.gaps {
            replay.peer_gap(&origin, gap, 1).unwrap();
            gaps.push((gap.first_missing, gap.last_missing));
        }
        for snapshot in batch.request.snapshots {
            replay.snapshot(&origin, snapshot, 1).unwrap();
        }
        for frame in batch.request.frames {
            replay.frame(&origin, frame.clone(), 1).unwrap();
            sent.push(frame.sequence);
        }
        through = batch.through;
    }
    assert_eq!(through, output.next_sequence() - 1);
    assert_eq!(gaps.last(), Some(&(2, 4)));
    assert!(sent.iter().all(|sequence| *sequence < 2));
    assert!(output.prepare(&peer, through, false).unwrap().is_none());
}

#[test]
fn node_reservation_and_empty_stream_epoch_are_bounded() {
    let mut streams = OutputStreams::default();
    let mut policy = policy();
    policy.replay_bytes = 1024 * 1024;
    for i in 0..32 {
        let mut t = terminal();
        t.terminal_id = Uuid::from_u128(100 + i);
        streams.attach(t, &policy).unwrap();
    }
    assert!(streams.attach(terminal(), &policy).is_err());
    let mut t = terminal();
    t.terminal_id = Uuid::from_u128(100);
    t.epoch += 1;
    assert!(streams.tail_mut(&t).is_err());
}

#[test]
fn output_rate_budgets_bound_flood_without_waiting() {
    let now = std::time::Instant::now();
    let mut session = Bucket::new(SESSION_BYTES_PER_SECOND, now);
    assert!(session.take(SESSION_BYTES_PER_SECOND, now));
    assert!(!session.take(1, now));
    assert!(session.take(
        SESSION_BYTES_PER_SECOND / 2,
        now + std::time::Duration::from_millis(500)
    ));
    assert!(!session.take(1, now + std::time::Duration::from_millis(500)));
    assert!(!session.take(1, now));
    let mut node = Bucket::new(NODE_BYTES_PER_SECOND, now);
    for _ in 0..8 {
        assert!(node.take(SESSION_BYTES_PER_SECOND, now));
    }
    assert!(!node.take(1, now));
    assert!(!node.take(
        NODE_BYTES_PER_SECOND + 1,
        now + std::time::Duration::from_secs(100)
    ));
}
