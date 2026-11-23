use super::*;
#[test]
fn partial_frame_consumes_only_the_complete_record() {
    let f = frame(b"{}\n{", 10, false).unwrap();
    assert_eq!((f.records.len(), f.consumed, f.partial), (1, 3, true));
    assert_eq!(f.records[0].end().unwrap(), 13);
}
#[test]
fn oversized_frame_ends_at_next_newline() {
    let f = frame(&vec![b'x'; MAX_RECORD_BYTES], 0, false).unwrap();
    assert!(f.records[0].oversized && f.discarding_oversized);
    let next = frame(b"tail\n{}\n", MAX_RECORD_BYTES as u64, true).unwrap();
    assert!(next.records[0].oversized);
    assert!(!next.records[1].oversized);
}
#[test]
fn frame_enforces_read_and_record_budgets() {
    assert!(frame(&vec![0; MAX_READ_BYTES + 1], 0, false).is_err());
    let many = b"{}\n".repeat(100);
    let f = frame(&many, 0, false).unwrap();
    assert_eq!(f.records.len(), MAX_RECORDS);
    assert!(f.consumed < many.len());
}
fn apply_stream(state: &mut Normalizer, bytes: &[u8]) -> Vec<ObservationRecord> {
    let (mut offset, mut discarding, mut out) = (0usize, false, vec![]);
    while offset < bytes.len() {
        let end = bytes.len().min(offset + MAX_READ_BYTES);
        let framed = frame(&bytes[offset..end], offset as u64, discarding).unwrap();
        assert!(framed.consumed > 0);
        for f in &framed.records {
            let (next, events) = state.apply(f, 100).unwrap();
            *state = next;
            out.extend(events);
        }
        offset += framed.consumed;
        discarding = framed.discarding_oversized;
    }
    out
}
fn oversized_record(prefix: &str, suffix: &str) -> Vec<u8> {
    let mut bytes = prefix.as_bytes().to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 2 * MAX_RECORD_BYTES + 4096));
    bytes.extend(suffix.as_bytes());
    bytes.push(b'\n');
    bytes
}
fn gaps(events: &[ObservationRecord]) -> Vec<String> {
    events
        .iter()
        .filter(|e| matches!(e.body.kind, ObservationKind::Gap { .. }))
        .map(|e| format!("{:?}", e.body.kind))
        .collect()
}
#[test]
fn claude_metadata_records_are_skipped_without_gaps() {
    let row = serde_json::to_vec(&fixtures()["claude"][0]).unwrap();
    let mut stream = vec![];
    for meta in [
        r#"{"type":"mode","mode":"default"}"#,
        r#"{"type":"api-request","sessionId":"native-fixture","requestId":"r1"}"#,
        r#"{"type":"system","subtype":"turn_duration","sessionId":"native-fixture","durationMs":1}"#,
        r#"{"type":"ai-title","title":"x"}"#,
    ] {
        stream.extend(meta.as_bytes());
        stream.push(b'\n');
    }
    stream.extend(&row);
    stream.push(b'\n');
    stream.extend(br#"{"type":"some-future-record","sessionId":"native-fixture"}"#);
    stream.push(b'\n');
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let events = apply_stream(&mut s, &stream);
    assert!(gaps(&events).is_empty(), "{:?}", gaps(&events));
    assert_eq!(usages(&events).len(), 1);

    // A metadata record naming another session is still refused.
    let s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let other = b"{\"type\":\"api-request\",\"sessionId\":\"another-session\"}\n";
    let framed = frame(other, 0, false).unwrap();
    assert!(s.apply(&framed.records[0], 100).is_err());
}

#[test]
fn claude_oversized_prompt_snapshot_is_skipped_but_other_oversized_records_gap() {
    let tail = r#""},"type":"attachment","uuid":"snap-1","sessionId":"native-fixture"}"#;
    let snapshot = r#"{"parentUuid":"54e0b998-9228-4f63-80a9-4d7023dcffda","isSidechain":false,"attachment":{"type":"prompt_snapshot","systemPrompt":""#;
    let row = serde_json::to_vec(&fixtures()["claude"][0]).unwrap();
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let mut stream = oversized_record(snapshot, tail);
    assert!(stream.len() > 2 * MAX_RECORD_BYTES);
    stream.extend(&row);
    stream.push(b'\n');
    let events = apply_stream(&mut s, &stream);
    assert!(gaps(&events).is_empty(), "{:?}", gaps(&events));
    assert_eq!(usages(&events).len(), 1, "facts after the snapshot survive");
    assert!(!s.context_gap && s.oversized_chain.is_none());

    let leading = r#"{"attachment":{"type":"prompt_snapshot","systemPrompt":""#;
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    assert!(gaps(&apply_stream(&mut s, &oversized_record(leading, tail))).is_empty());

    for (format, prefix) in [
        (
            Format::ClaudeV1Jsonl9a9b8e8,
            r#"{"parentUuid":null,"attachment":{"type":"hook_output","text":""#,
        ),
        (
            Format::ClaudeV1Jsonl9a9b8e8,
            r#"{"type":"user","message":{"attachment":{"type":"prompt_snapshot","text":""#,
        ),
        (
            Format::ClaudeV1Jsonl9a9b8e8,
            r#"{"parentUuid":"x{","attachment":{"type":"prompt_snapshot","text":""#,
        ),
        (Format::CodexRollout67f0994, snapshot),
    ] {
        let mut s = Normalizer::new(source(format.clone()), 128).unwrap();
        let events = apply_stream(&mut s, &oversized_record(prefix, tail));
        assert_eq!(
            gaps(&events).len(),
            3,
            "{format:?} {prefix}: every frame gaps"
        );
        assert!(s.context_gap && s.oversized_chain.is_none());
    }
}

