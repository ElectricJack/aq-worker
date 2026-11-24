use aq_worker::native::adapter::codex::{CodexEvent, CodexEvents};
const MAX_EVENT: usize = 32_768;
#[test]
fn jsonl_readiness_requires_actual_order_and_handles_chunk_boundaries() {
    let id = uuid::Uuid::new_v4();
    let mut p = CodexEvents::default();
    let first = format!("{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}\r\n");
    assert!(p.push(&first.as_bytes()[..10]).unwrap().is_empty());
    assert_eq!(
        p.push(&first.as_bytes()[10..]).unwrap(),
        vec![CodexEvent::ThreadStarted(id)]
    );
    assert_eq!(
        p.push(b"{\"type\":\"turn.started\"}\n").unwrap(),
        vec![CodexEvent::TurnStarted(id)]
    );
    p.push(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}\n")
        .unwrap();
    assert!(p.finish().is_err());
    p.push(b"{\"type\":\"turn.completed\"}\n").unwrap();
    p.finish().unwrap();
    let mut p = CodexEvents::default();
    assert!(p.push(b"{\"type\":\"turn.started\"}\n").is_err());
    assert!(p.push(first.as_bytes()).is_err());
    let mut p = CodexEvents::default();
    p.push(&vec![b'x'; MAX_EVENT]).unwrap();
    assert!(p.push(b"x").is_err());
    assert!(p.push(first.as_bytes()).is_err());
}

#[test]
fn fatal_or_empty_stream_cannot_report_completion_or_later_readiness() {
    let mut p = CodexEvents::default();
    assert!(p.finish().is_err());
    let id = uuid::Uuid::new_v4();
    let events = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}\n{{\"type\":\"error\"}}\n{{\"type\":\"turn.started\"}}\n"
    );
    assert_eq!(
        p.push(events.as_bytes()).unwrap(),
        vec![CodexEvent::ThreadStarted(id), CodexEvent::Failed]
    );
    assert!(p.finish().is_err());
    let mut p = CodexEvents::default();
    assert!(p.push(&vec![b'x'; MAX_EVENT + 1]).is_err());
    assert!(p.push(events.as_bytes()).is_err());
}
