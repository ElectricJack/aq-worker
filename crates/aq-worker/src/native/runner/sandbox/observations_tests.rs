use super::observations::*;
use aq_observation::lifecycle::*;
use aq_observation_engine::{Format, Source};
use std::{fs, io::Write, path::Path};
use uuid::Uuid;

const REAL: &[u8] = include_bytes!("../../../../tests/fixtures/codex-real-0.166.0.jsonl");
fn source() -> Source {
    Source {
        binding: Binding {
            project_id: Uuid::from_u128(1),
            task_id: Uuid::from_u128(2),
            lineage_id: Uuid::from_u128(3),
            attempt_id: Uuid::from_u128(4),
            attempt_generation: 1,
            session: SessionIdentity::Remote {
                node_id: Uuid::from_u128(5),
                boot_id: Uuid::from_u128(6),
                session_id: Uuid::from_u128(7),
                attempt_id: Uuid::from_u128(4),
                assignment_generation: 1,
            },
        },
        source_id: Uuid::from_u128(8),
        generation: 1,
        native_session_key: Text::new("sanitized-h1-session").unwrap(),
        format: Format::CodexRollout67f0994,
        provider: Text::new("openai").unwrap(),
        account_reference: Text::new("logical-account").unwrap(),
        registered_session_key: None,
    }
}
fn open(root: &Path, native: &Path) -> anyhow::Result<Collector> {
    Collector::open(
        root,
        source(),
        native,
        "rollout.jsonl",
        &[native.to_owned()],
    )
}
fn receipt(packet: &Packet) -> Receipt {
    let Message::Batch(batch) = &packet.body.message else {
        panic!()
    };
    Receipt {
        schema_version: SCHEMA_VERSION,
        message_id: packet.body.message_id,
        body_digest: packet.body_digest.clone(),
        disposition: ReceiptDisposition::ObservationRecorded,
        through_sequence: Some(batch.last_sequence),
    }
}
#[test]
fn observation_cursor_survives_runner_restart_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    let path = native.join("rollout.jsonl");
    fs::write(&path, REAL).unwrap();
    let root = temp.path().join("observations");
    let collector = open(&root, &native).unwrap();
    while collector.collect(100).unwrap().consumed_bytes > 0 {}
    let first = collector.prepare().unwrap().unwrap();
    let bytes = serde_json::to_vec(&first).unwrap();
    let cursor = collector.cursor().unwrap();
    assert_eq!(cursor, REAL.len() as u64);
    let Message::Batch(batch) = &first.body.message else {
        panic!()
    };
    let ids: std::collections::BTreeSet<_> = batch.events.iter().map(|e| e.event_id).collect();
    assert_eq!(ids.len(), batch.events.len());
    assert!(
        batch
            .events
            .iter()
            .any(|e| matches!(e.body.kind, ObservationKind::Usage { .. }))
    );
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("[sanitized]")
    );
    drop(collector); // Runner dies after local commit/send, before receipt.
    let collector = open(&root, &native).unwrap();
    assert_eq!(collector.cursor().unwrap(), cursor);
    assert_eq!(collector.collect(101).unwrap().facts_added, 0);
    assert_eq!(
        serde_json::to_vec(&collector.prepare().unwrap().unwrap()).unwrap(),
        bytes
    );
    let mut wrong = receipt(&first);
    wrong.body_digest = Digest::of(b"wrong");
    assert!(collector.acknowledge(&wrong).is_err());
    assert_eq!(
        serde_json::to_vec(&collector.prepare().unwrap().unwrap()).unwrap(),
        bytes
    );
    collector.acknowledge(&receipt(&first)).unwrap();
    drop(collector);
    let collector = open(&root, &native).unwrap();
    assert!(collector.prepare().unwrap().is_none());
    // Same native event at another byte position cannot multiply usage.
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(REAL)
        .unwrap();
    while collector.collect(102).unwrap().consumed_bytes > 0 {}
    if let Some(packet) = collector.prepare().unwrap() {
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        assert!(batch.events.iter().all(|e| !ids.contains(&e.event_id)));
        assert!(
            batch
                .events
                .iter()
                .all(|e| !matches!(e.body.kind, ObservationKind::Usage { .. }))
        );
    }
    collector.audit().unwrap();
}

#[test]
fn observation_paths_identity_partial_and_loss_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    let path = native.join("rollout.jsonl");
    fs::write(&path, REAL).unwrap();
    let root = temp.path().join("rows");
    assert!(
        Collector::open(
            &root,
            source(),
            &native,
            "rollout.jsonl",
            &[temp.path().join("other")]
        )
        .is_err()
    );
    std::os::unix::fs::symlink(&path, native.join("link.jsonl")).unwrap();
    assert!(
        Collector::open(
            &root,
            source(),
            &native,
            "link.jsonl",
            std::slice::from_ref(&native)
        )
        .is_err()
    );
    let mut foreign = source();
    foreign.native_session_key = Text::new("foreign").unwrap();
    assert!(
        Collector::open(
            &root,
            foreign,
            &native,
            "rollout.jsonl",
            std::slice::from_ref(&native)
        )
        .is_err()
    );
    // Failed admission must leave no partially attached store.
    let collector = open(&root, &native).unwrap();
    while collector.collect(100).unwrap().consumed_bytes > 0 {}
    let before = collector.cursor().unwrap();
    let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
    append.write_all(b"{\"type\":").unwrap();
    assert_eq!(collector.collect(101).unwrap().consumed_bytes, 0);
    assert_eq!(collector.cursor().unwrap(), before);
    append.write_all(&vec![b'x'; 64 * 1024]).unwrap();
    assert!(collector.collect(102).unwrap().facts_added > 0);
    let packet = collector.prepare().unwrap().unwrap();
    let Message::Batch(batch) = &packet.body.message else {
        panic!()
    };
    assert!(
        batch
            .events
            .iter()
            .any(|e| matches!(e.body.kind, ObservationKind::Gap { .. }))
    );
    fs::rename(&path, native.join("old")).unwrap();
    fs::write(&path, REAL).unwrap();
    let gap = collector.collect(103).unwrap();
    assert!(gap.blocked);
    assert_eq!(gap.facts_added, 1);
    assert_eq!(collector.collect(104).unwrap().facts_added, 0);
    collector.audit().unwrap();
}

#[test]
fn observation_overload_commits_loss_gap_with_cursor_and_holds() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    let mut bytes = REAL.to_vec();
    for i in 0..300 {
        bytes.extend_from_slice(format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"overload-{i}\"}}}}\n").as_bytes());
    }
    fs::write(native.join("rollout.jsonl"), &bytes).unwrap();
    let root = temp.path().join("rows");
    let collector = open(&root, &native).unwrap();
    let mut held = false;
    for _ in 0..20 {
        let result = collector.collect(100).unwrap();
        if result.blocked {
            held = true;
            break;
        }
    }
    assert!(held);
    let cursor = collector.cursor().unwrap();
    assert!(cursor < bytes.len() as u64);
    assert_eq!(collector.collect(101).unwrap().consumed_bytes, 0);
    let mut found_gap = false;
    while let Some(packet) = collector.prepare().unwrap() {
        assert!(serde_json::to_vec(&packet).unwrap().len() <= 256 * 1024);
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        for event in &batch.events {
            assert!(serde_json::to_vec(event).unwrap().len() <= 64 * 1024);
            found_gap |= matches!(event.body.kind, ObservationKind::Gap { .. });
        }
        collector.acknowledge(&receipt(&packet)).unwrap();
    }
    assert!(found_gap);
    collector.audit().unwrap();
    drop(collector);
    let collector = open(&root, &native).unwrap();
    assert_eq!(collector.cursor().unwrap(), cursor);
    assert!(collector.collect(102).unwrap().blocked);
}

#[test]
fn observation_replays_pending_when_exact_source_disappears() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("rollout.jsonl"), REAL).unwrap();
    let root = temp.path().join("rows");
    let collector = open(&root, &native).unwrap();
    while collector.collect(100).unwrap().consumed_bytes > 0 {}
    let pending = serde_json::to_vec(&collector.prepare().unwrap().unwrap()).unwrap();
    let cursor = collector.cursor().unwrap();
    drop(collector);
    fs::remove_dir_all(&native).unwrap();
    let collector = open(&root, &native).unwrap();
    assert!(collector.collect(101).is_err());
    assert_eq!(collector.cursor().unwrap(), cursor);
    assert_eq!(
        serde_json::to_vec(&collector.prepare().unwrap().unwrap()).unwrap(),
        pending
    );
}

#[test]
fn observation_reader_requires_explicit_launched_file_owner() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("rollout.jsonl"), REAL).unwrap();
    let owner = unsafe { libc::geteuid() };
    let permit = aq_observation::source::SourceFile::inspect_owned(
        Uuid::from_u128(8),
        temp.path(),
        "rollout.jsonl",
        Text::new("sanitized-h1-session").unwrap(),
        owner,
    )
    .unwrap();
    aq_observation::source::SourceFile::open(permit.clone()).unwrap();
    let mut wrong = permit;
    wrong.owner_uid = Some(owner + 1);
    assert!(aq_observation::source::SourceFile::open(wrong).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observation_permit_delivery_and_held_upload_never_block_control() {
    use super::{auth_tests::client, channels::SandboxChannels};
    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, header},
        routing::post,
    };
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Notify, Semaphore};
    #[derive(Clone)]
    struct Peer {
        arrived: Arc<Notify>,
        release: Arc<Semaphore>,
        seen: Arc<Mutex<Vec<(HeaderMap, Packet)>>>,
    }
    async fn upload(State(peer): State<Peer>, headers: HeaderMap, bytes: Bytes) -> Json<Receipt> {
        let packet: Packet = serde_json::from_slice(&bytes).unwrap();
        packet.validate().unwrap();
        peer.seen.lock().unwrap().push((headers, packet.clone()));
        peer.arrived.notify_one();
        peer.release.acquire().await.unwrap().forget();
        Json(receipt(&packet))
    }
    let _clock = crate::native::http::test_clock::TestClock::new();
    let peer = Peer {
        arrived: Arc::new(Notify::new()),
        release: Arc::new(Semaphore::new(0)),
        seen: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/worker/observations/batch", post(upload))
        .with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_private, http, permit) = client(&origin, false);
    let mut src = source();
    src.binding.session = http.launch_binding().session;
    src.binding.attempt_id = permit.binding.attempt_id;
    src.binding.attempt_generation = http.launch_binding().assignment_generation;
    let intent = aq_session_host::LaunchIntent {
        launch_id: src.source_id,
        session: src.binding.session.clone(),
        manifest_digest: Digest::of(b"manifest"),
        configuration_digest: Digest::of(b"config"),
    };
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("rollout.jsonl"), REAL).unwrap();
    let transcript = aq_session_host::TranscriptSource {
        launch: intent.clone(),
        root: native.clone(),
        member: "rollout.jsonl".into(),
        native_session_key: src.native_session_key.clone(),
        format_id: "codex-rollout-jsonl-67f0994".into(),
        provider: src.provider.clone(),
        account_reference: src.account_reference.clone(),
    };
    fs::set_permissions(
        temp.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let root = temp.path().join("rows");
    let mut wrong = transcript.clone();
    wrong.launch.launch_id = Uuid::new_v4();
    assert!(
        ObservationTask::start(
            root.clone(),
            src.clone(),
            wrong,
            &intent,
            vec![native.clone()],
            SandboxChannels::new(&http).unwrap(),
            unsafe { libc::geteuid() }
        )
        .is_err()
    );
    let mut task = ObservationTask::start(
        root.clone(),
        src.clone(),
        transcript.clone(),
        &intent,
        vec![native.clone()],
        SandboxChannels::new(&http).unwrap(),
        unsafe { libc::geteuid() },
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                _ = peer.arrived.notified() => break,
                changed = task.status.changed() => {
                    if changed.is_err() {
                        task.stop();
                        panic!("observation worker exited; last status: {:?}", *task.status.borrow());
                    }
                    assert!(!matches!(*task.status.borrow(),
                        ObservationStatus::SourceUnavailable | ObservationStatus::StoreUnavailable |
                        ObservationStatus::DeliveryUnavailable), "collector status: {:?}", *task.status.borrow());
                }
            }
        }
    }).await.expect("observation peer watchdog");
    {
        let seen = peer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].0[header::AUTHORIZATION],
            format!("Bearer {}", permit.credential.expose())
        );
    }
    // Control proceeds while the peer is held: cancellation closes the telemetry
    // future without waiting for the receipt, and the durable packet survives.
    task.stop();
    task.wait().await.unwrap();
    let collector = Collector::open_for_owner(
        &root,
        src.clone(),
        &native,
        "rollout.jsonl",
        std::slice::from_ref(&native),
        unsafe { libc::geteuid() },
    )
    .unwrap();
    let pending = collector.prepare().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&pending).unwrap(),
        serde_json::to_vec(&peer.seen.lock().unwrap()[0].1).unwrap()
    );
    drop(collector);
    peer.release.add_permits(2);
    let mut task = ObservationTask::start(
        root.clone(),
        src.clone(),
        transcript,
        &intent,
        vec![native.clone()],
        SandboxChannels::new(&http).unwrap(),
        unsafe { libc::geteuid() },
    )
    .unwrap();
    loop {
        if *task.status.borrow() == ObservationStatus::Idle {
            break;
        }
        task.status.changed().await.unwrap();
    }
    task.stop();
    task.wait().await.unwrap();
    let collector = Collector::open_for_owner(
        &root,
        src,
        &native,
        "rollout.jsonl",
        std::slice::from_ref(&native),
        unsafe { libc::geteuid() },
    )
    .unwrap();
    assert!(collector.prepare().unwrap().is_none());
    let seen = peer.seen.lock().unwrap();
    assert_eq!(
        serde_json::to_vec(&seen[0].1).unwrap(),
        serde_json::to_vec(&seen[1].1).unwrap()
    );
    server.abort();
}

#[test]
fn codex_real_rollout_0166_qualifies_reference_and_durable_engine() {
    let reference_source = serde_json::from_value(serde_json::to_value(source()).unwrap()).unwrap();
    let mut normalizer = aq_observation::Normalizer::new(reference_source, 4096).unwrap();
    let frames = aq_observation::framing::frame(REAL, 0, false).unwrap();
    let mut reference = vec![];
    for frame in frames.records {
        let (next, facts) = normalizer.apply(&frame, 100).unwrap();
        normalizer = next;
        reference.extend(facts);
    }
    assert!(
        !reference
            .iter()
            .any(|e| matches!(e.body.kind, ObservationKind::Gap { .. }))
    );
    let usages: Vec<_> = reference
        .iter()
        .filter_map(|e| {
            if let ObservationKind::Usage { usage } = &e.body.kind {
                Some(usage)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(usages.len(), 4);
    for (usage, (input, output, cache_read, reasoning)) in usages.iter().zip([
        (15928, 109, 12032, 0),
        (41602, 323, 27904, 0),
        (73534, 720, 53632, 53),
        (115950, 1060, 85760, 66),
    ]) {
        assert_eq!(usage.counters.input, Counter::Known { value: input });
        assert_eq!(usage.counters.output, Counter::Known { value: output });
        assert_eq!(
            usage.counters.cache_read,
            Counter::Known { value: cache_read }
        );
        assert_eq!(usage.counters.cache_write, Counter::Known { value: 0 });
        assert_eq!(
            usage.counters.reasoning,
            Counter::Known { value: reasoning }
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("rollout.jsonl"), REAL).unwrap();
    let collector = open(&temp.path().join("rows"), &native).unwrap();
    while collector.collect(100).unwrap().consumed_bytes > 0 {}
    let packet = collector.prepare().unwrap().unwrap();
    let Message::Batch(batch) = &packet.body.message else {
        panic!()
    };
    assert_eq!(batch.events.len(), reference.len());
    for (engine, reference) in batch.events.iter().zip(reference) {
        match (&engine.body.kind, &reference.body.kind) {
            (ObservationKind::Usage { usage: a }, ObservationKind::Usage { usage: b }) => {
                assert_eq!(a.call, b.call);
                assert_eq!(a.basis, b.basis);
                assert_eq!(a.counters, b.counters);
                assert_eq!(a.cost, b.cost);
                assert_eq!(a.completeness, b.completeness);
                assert_eq!(a.correction_of, b.correction_of);
                // Each pinned parser intentionally identifies its own semantics artifact.
                assert!(matches!(
                    a.category_semantics,
                    CategorySemantics::MayOverlap { .. }
                ));
                assert!(matches!(
                    b.category_semantics,
                    CategorySemantics::MayOverlap { .. }
                ));
            }
            _ => assert_eq!(engine.body.kind, reference.body.kind),
        }
        assert_eq!(
            engine.body.provenance.cursor,
            reference.body.provenance.cursor
        );
    }
}

fn catalog_entry(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: serde_json::from_value(serde_json::json!("ab".repeat(32))).unwrap(),
    }
}

#[test]
fn profiled_source_id_matches_server_derivation_vector() {
    let entry = catalog_entry("aq.observation-catalog/codex");
    let id = profiled_source_id(Uuid::from_u128(4), 1, &entry).unwrap();
    // Independently computed: SHA-256 of the sorted-key compact JSON
    // ["aq.observation-source/v1","00000000-0000-0000-0000-000000000004",1,
    //  {"digest":"abab..","id":"aq.observation-catalog/codex","version":"1"}],
    // first 16 bytes with version 8 / RFC 9562 variant bits.
    assert_eq!(id.to_string(), "2070c132-6c63-8df1-925e-6ebd36112419");
    assert_eq!(id.get_version_num(), 8);
    assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(
        profiled_source_id(Uuid::from_u128(4), 1, &entry).unwrap(),
        id
    );
    for other in [
        profiled_source_id(Uuid::from_u128(5), 1, &entry).unwrap(),
        profiled_source_id(Uuid::from_u128(4), 2, &entry).unwrap(),
        profiled_source_id(Uuid::from_u128(4), 1, &catalog_entry("other")).unwrap(),
    ] {
        assert_ne!(other, id);
    }
    assert_eq!(
        profiled_session_key(&source().binding).unwrap().as_str(),
        "aq.attempt/00000000-0000-0000-0000-000000000004/1"
    );
}

#[test]
fn profiled_source_emits_server_identity_while_parsing_native_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("rollout.jsonl"), REAL).unwrap();
    let mut profiled = source();
    let entry = catalog_entry("aq.observation-catalog/codex");
    profiled.source_id = profiled_source_id(profiled.binding.attempt_id, 1, &entry).unwrap();
    profiled.registered_session_key = Some(profiled_session_key(&profiled.binding).unwrap());
    profiled.validate().unwrap();
    let collector = Collector::open(
        &temp.path().join("observations"),
        profiled.clone(),
        &native,
        "rollout.jsonl",
        std::slice::from_ref(&native),
    )
    .unwrap();
    while collector.collect(100).unwrap().consumed_bytes > 0 {}
    let packet = collector.prepare().unwrap().unwrap();
    let Message::Batch(batch) = &packet.body.message else {
        panic!()
    };
    assert!(!batch.events.is_empty());
    let mut usage = 0;
    for event in &batch.events {
        let provenance = &event.body.provenance;
        assert_eq!(provenance.source_id, profiled.source_id);
        assert_eq!(provenance.source_generation, 1);
        assert_eq!(
            provenance.native_session_key.as_str(),
            "aq.attempt/00000000-0000-0000-0000-000000000004/1"
        );
        if let ObservationKind::Usage { usage: u } = &event.body.kind {
            assert_eq!(u.call.account_reference.as_str(), "logical-account");
            usage += 1;
        }
    }
    assert!(usage > 0);
    collector.audit().unwrap();
}
