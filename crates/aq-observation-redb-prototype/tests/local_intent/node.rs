//! The node observation service against an in-memory server that applies the
//! real transport contract and the ledger rule `first_sequence == through + 1`.
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation_admission::{
    self as a, BatchReply, Operation, Owner, RefusalReason, RefusedSource, Reply, Request, Status,
};
use aq_observation_engine::Format;
use aq_observation_redb_prototype::{
    AuthenticatedOwnerPeer, LocalIntent,
    node::{Node, NodeLimits, NodePeer, Observations, SourceSpec, SourceStatus},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const NODE: Uuid = Uuid::from_u128(5);
#[derive(Default)]
struct State {
    boot: Uuid,
    down: bool,
    /// Commit the next batch, lose its receipt and stay unreachable.
    lose: bool,
    owner: Option<Owner>,
    receipts: BTreeMap<Uuid, a::Receipt>,
    boots: BTreeSet<Uuid>,
    calls: Vec<&'static str>,
    through: u64,
    acked: BTreeMap<Uuid, BatchReply>,
    /// Sources whose records the server drops and lists as refused.
    refuse: BTreeSet<Uuid>,
    events: Vec<ObservationRecord>,
    /// Every upload as sent, retries included.
    posts: Vec<Vec<u8>>,
}
#[derive(Clone, Default)]
struct Server(Arc<Mutex<State>>);
impl Server {
    fn on(boot: u128) -> Self {
        let server = Self::default();
        server.0.lock().unwrap().boot = Uuid::from_u128(boot);
        server
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
}
impl AuthenticatedOwnerPeer for Server {
    fn mutate(&mut self, r: &Request) -> Result<Reply> {
        let mut s = self.state();
        ensure!(!s.down, "unreachable");
        let (kind, seen) = match r.operation {
            Operation::Prepare { .. } => ("prepare", false),
            Operation::Activate { .. } => ("activate", false),
            Operation::AdvanceBoot { new_boot_id, .. } => ("boot", s.boots.contains(&new_boot_id)),
        };
        let access = a::Access {
            node_id: NODE,
            boot_id: s.boot,
            accepted_through: s.through,
            max_pending_distance: a::MAX_PENDING_DISTANCE,
            new_boot_previously_owned: seen,
        };
        let t = a::apply(
            s.owner.as_ref(),
            s.receipts.get(&r.operation_id),
            r,
            &access,
            Some(Uuid::from_u128(400)),
        )?;
        if t.changed {
            s.calls.push(kind);
        }
        s.owner = Some(t.reply.current_owner.clone());
        s.boots.insert(t.reply.current_owner.current_boot_id);
        s.receipts.insert(r.operation_id, t.reply.receipt.clone());
        Ok(t.reply)
    }
    fn status(&mut self, node: Uuid) -> Result<Status> {
        let s = self.state();
        ensure!(!s.down, "unreachable");
        Ok(Status {
            version: 1,
            node_id: node,
            admitted_node_boot_id: s.boot,
            owner: s.owner.clone(),
            accepted_through: s.through,
        })
    }
}
impl NodePeer for Server {
    fn batch(&mut self, packet: &Packet) -> Result<BatchReply> {
        let mut s = self.state();
        ensure!(!s.down, "unreachable");
        s.posts.push(serde_json::to_vec(packet)?);
        packet.validate()?;
        let Message::Batch(batch) = &packet.body.message else {
            anyhow::bail!("packet class")
        };
        ensure!(
            s.owner
                .as_ref()
                .is_some_and(|o| o.phase == a::Phase::Active),
            "transport owner not active"
        );
        if let Some(old) = s.acked.get(&packet.body.message_id) {
            return Ok(old.clone());
        }
        ensure!(batch.first_sequence == s.through + 1, "sequence conflict");
        // aq-server: a refused source loses its records, the batch still lands.
        let refused: BTreeSet<Uuid> = (batch.events.iter())
            .map(|e| e.body.provenance.source_id)
            .filter(|id| s.refuse.contains(id))
            .collect();
        let receipt = BatchReply {
            receipt: super::support::receipt(packet),
            refused_sources: (refused.iter())
                .map(|id| RefusedSource {
                    source_id: *id,
                    generation: 1,
                    reason: RefusalReason::Parser,
                })
                .collect(),
        };
        s.through = batch.last_sequence;
        s.events.extend(
            (batch.events.iter())
                .filter(|e| !refused.contains(&e.body.provenance.source_id))
                .cloned(),
        );
        s.acked.insert(packet.body.message_id, receipt.clone());
        if s.lose {
            s.lose = false;
            s.down = true;
            anyhow::bail!("receipt lost");
        }
        Ok(receipt)
    }
}

fn entry() -> ArtifactRef {
    serde_json::from_value(
        json!({"id":"aq.observation-catalog/codex","version":"1","digest":"ab".repeat(32)}),
    )
    .unwrap()
}
fn spec(attempt: u128, boot: u128, transcript: &Path, key: &str) -> SourceSpec {
    let attempt = Uuid::from_u128(attempt);
    SourceSpec {
        binding: serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":attempt,"attempt_generation":7,"session":{"kind":"remote","node_id":NODE,"boot_id":Uuid::from_u128(boot),"session_id":Uuid::from_u128(7),"attempt_id":attempt,"assignment_generation":7}})).unwrap(),
        catalog_entry: entry(),
        format: Format::ClaudeV1Jsonl9a9b8e8,
        transcript: transcript.into(),
        authorized_root: transcript.parent().unwrap().into(),
        native_session_key: key.into(),
        provider: "anthropic".into(),
        account_reference: "owner-claude".into(),
        owner_uid: None,
    }
}
/// One Claude session line: assistant text and a tool call, or the tool's result.
fn line(key: &str, n: u32, result: bool) -> Vec<u8> {
    let message = if result {
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":format!("t{}",n-1),"content":"ok"}]})
    } else {
        json!({"id":format!("m{n}"),"model":"model-a","content":[{"type":"text","text":format!("step {n}")},{"type":"tool_use","id":format!("t{n}"),"name":"Bash","input":{}}]})
    };
    let kind = if result { "user" } else { "assistant" };
    let mut bytes = serde_json::to_vec(
        &json!({"type":kind,"uuid":format!("{key}-{n}"),"sessionId":key,"message":message}),
    )
    .unwrap();
    bytes.push(b'\n');
    bytes
}
fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
/// A private transcript directory, as the launch will mount one per attempt.
fn transcripts(temp: &tempfile::TempDir, name: &str) -> PathBuf {
    let directory = std::fs::canonicalize(temp.path()).unwrap().join(name);
    std::fs::create_dir(&directory).unwrap();
    directory
}
type Log = Arc<Mutex<Vec<String>>>;
fn start(temp: &tempfile::TempDir, boot: u128, server: &Server) -> (Node, Observations, Log) {
    start_with(temp, boot, server, NodeLimits::default())
}
fn start_with(
    temp: &tempfile::TempDir,
    boot: u128,
    server: &Server,
    limits: NodeLimits,
) -> (Node, Observations, Log) {
    let log = Log::default();
    let lines = log.clone();
    let limits = NodeLimits {
        poll: Duration::from_millis(5),
        retry_start: Duration::from_millis(5),
        retry_max: Duration::from_millis(20),
        finish_drain: Duration::from_secs(2),
        ..limits
    };
    let node = Node::start(
        &temp.path().join("observations"),
        NODE,
        Uuid::from_u128(boot),
        limits,
        server.clone(),
        move |line| lines.lock().unwrap().push(line),
    )
    .unwrap();
    let observations = node.observations();
    (node, observations, log)
}
#[track_caller]
fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn sequences(server: &Server) -> Vec<u64> {
    (server.state().events.iter())
        .map(|e| e.body.sequence)
        .collect()
}
fn kind(event: &ObservationRecord) -> String {
    serde_json::to_value(&event.body.kind).unwrap()["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// aq-server `aq_admission::observation_source::source_identity_matches_the_worker_vector`.
#[test]
fn source_identity_is_the_servers_vector_at_generation_one() {
    let source = spec(4, 6, Path::new("/slot/transcripts/k.jsonl"), "k")
        .source()
        .unwrap();
    assert_eq!(
        source.source_id.to_string(),
        "2070c132-6c63-8df1-925e-6ebd36112419"
    );
    // The binding's assignment generation is 7; the source generation is not.
    assert_eq!(source.generation, 1);
    assert_eq!(
        source.provenance_session_key().as_str(),
        "aq.attempt/00000000-0000-0000-0000-000000000004/1"
    );
    let mut outside = spec(4, 6, Path::new("/slot/transcripts/k.jsonl"), "k");
    outside.authorized_root = "/slot/other".into();
    assert!(outside.source().is_err());
    let mut gemini = spec(4, 6, Path::new("/slot/transcripts/k.jsonl"), "k");
    gemini.format = Format::GeminiRecording6814e00;
    assert!(gemini.source().is_err());
}

#[test]
fn two_sources_share_one_gapless_sequence_with_the_progress_kinds_the_ladder_counts() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let (node, observations, _) = start(&temp, 6, &server);
    // One source names its file, the other the directory that will hold it.
    let (one, two) = (transcripts(&temp, "one"), transcripts(&temp, "two"));
    let first = observations
        .register(spec(4, 6, &one.join("s-one.jsonl"), "s-one"))
        .unwrap();
    let second = observations.register(spec(9, 6, &two, "s-two")).unwrap();
    assert!(
        observations.register(spec(9, 6, &two, "s-two")).is_err(),
        "one source per attempt"
    );
    for n in [1, 3, 5] {
        for (directory, key) in [(&one, "s-one"), (&two, "s-two")] {
            let path = directory.join(format!("{key}.jsonl"));
            append(&path, &line(key, n, false));
            append(&path, &line(key, n + 1, true));
        }
        // text, tool start and tool result per pair of lines, per source
        wait("both sources delivered", || {
            server.state().events.len() >= (n as usize).div_ceil(2) * 6
        });
    }
    let state = server.state();
    let expected: Vec<u64> = (1..=state.events.len() as u64).collect();
    assert_eq!(
        state
            .events
            .iter()
            .map(|e| e.body.sequence)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(state.calls, ["prepare", "activate"]);
    for handle in [&first, &second] {
        assert_eq!(handle.status(), Some(SourceStatus::Collecting));
        let own: Vec<_> = (state.events.iter())
            .filter(|e| e.body.provenance.source_id == handle.source_id())
            .collect();
        assert_eq!(own.len(), 9);
        // aq-server `aq_stall_ladder::evidence::PROGRESS_SQL`: tool_started,
        // tool_result, and text_reference with role assistant.
        let kinds: BTreeSet<String> = own.iter().map(|e| kind(e)).collect();
        assert_eq!(
            kinds.into_iter().collect::<Vec<_>>(),
            ["text_reference", "tool_result", "tool_started"]
        );
        assert!(own.iter().any(|e| {
            let kind = serde_json::to_value(&e.body.kind).unwrap();
            kind["kind"] == "text_reference" && kind["role"] == "assistant"
        }));
        let attempt = own[0].body.binding.attempt_id;
        assert!(own.iter().all(|e| {
            e.body.provenance.source_generation == 1
                && e.body.provenance.native_session_key.as_str()
                    == format!("aq.attempt/{attempt}/1")
        }));
    }
    // Interleaved: neither source's events form one contiguous run.
    let owners: Vec<Uuid> = (state.events.iter())
        .map(|e| e.body.provenance.source_id)
        .collect();
    assert!(owners.windows(2).filter(|w| w[0] != w[1]).count() >= 3);
    drop(state);
    node.shutdown();
}

#[test]
fn a_missing_or_refused_transcript_is_incomplete_and_never_delays_the_others() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let (node, observations, log) = start(&temp, 6, &server);
    let (good, absent, foreign, linked) = (
        transcripts(&temp, "good"),
        transcripts(&temp, "absent"),
        transcripts(&temp, "foreign"),
        transcripts(&temp, "linked"),
    );
    // Never written; another session's file; a link out of the authorized root.
    let missing = observations
        .register(spec(11, 6, &absent.join("never.jsonl"), "never"))
        .unwrap();
    append(&foreign.join("mine.jsonl"), &line("someone-else", 1, false));
    let refused = observations
        .register(spec(12, 6, &foreign.join("mine.jsonl"), "mine"))
        .unwrap();
    std::os::unix::fs::symlink(good.join("ok.jsonl"), linked.join("ok.jsonl")).unwrap();
    let escaping = observations
        .register(spec(13, 6, &linked.join("ok.jsonl"), "ok"))
        .unwrap();
    let healthy = observations
        .register(spec(4, 6, &good.join("ok.jsonl"), "ok"))
        .unwrap();
    let started = Instant::now();
    append(&good.join("ok.jsonl"), &line("ok", 1, false));
    wait("the healthy source", || server.state().events.len() == 2);
    assert!(started.elapsed() < Duration::from_secs(5));
    wait("refusals", || {
        matches!(refused.status(), Some(SourceStatus::Incomplete(_)))
            && matches!(escaping.status(), Some(SourceStatus::Incomplete(_)))
    });
    assert_eq!(missing.status(), Some(SourceStatus::Waiting));
    assert_eq!(healthy.status(), Some(SourceStatus::Collecting));
    // The healthy source keeps flowing past the broken ones.
    append(&good.join("ok.jsonl"), &line("ok", 2, true));
    wait("more healthy events", || server.state().events.len() == 3);
    let id = missing.source_id();
    drop(missing);
    wait("the missing transcript recorded incomplete", || {
        (log.lock().unwrap().iter())
            .any(|l| l.contains(&id.to_string()) && l.contains("never appeared"))
    });
    assert!(
        (server.state().events.iter()).all(|e| e.body.provenance.source_id == healthy.source_id())
    );
    assert_eq!(sequences(&server), [1, 2, 3]);
    node.shutdown();
}

#[test]
fn a_restart_resends_only_the_unacknowledged_batch_and_resumes_the_surviving_source() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    server.state().down = true;
    let (node, observations, log) = start(&temp, 6, &server);
    let old = transcripts(&temp, "old");
    let first = observations
        .register(spec(4, 6, &old.join("a.jsonl"), "a"))
        .unwrap();
    append(&old.join("a.jsonl"), &line("a", 1, false));
    // Unreachable at start: retried, and nothing was lost meanwhile.
    wait("a refused handshake", || {
        (log.lock().unwrap().iter()).any(|l| l.contains("not active"))
    });
    assert!(server.state().calls.is_empty());
    server.state().down = false;
    wait("the acknowledged batch", || {
        server.state().events.len() == 2
    });
    server.state().lose = true;
    append(&old.join("a.jsonl"), &line("a", 2, true));
    wait("the batch whose receipt is lost", || server.state().down);
    node.shutdown();
    let first_id = first.source_id();
    drop(first);
    assert_eq!(sequences(&server), [1, 2, 3]);
    assert_eq!(server.state().calls, ["prepare", "activate"]);

    // The worker restarts: the server admits a new boot of the node.
    {
        let mut state = server.state();
        state.boot = Uuid::from_u128(60);
        state.down = false;
    }
    let (node, observations, _) = start(&temp, 60, &server);
    let new = transcripts(&temp, "new");
    let second = observations
        .register(spec(9, 60, &new.join("b.jsonl"), "b"))
        .unwrap();
    append(&new.join("b.jsonl"), &line("b", 1, false));
    wait("the new boot's source", || {
        (server.state().events.iter()).any(|e| e.body.provenance.source_id == second.source_id())
    });
    // The attempt of the ended boot survived: registering the same source
    // again resumes it from its stored cursor, under the same source ID.
    let resumed = observations
        .register(spec(4, 6, &old.join("a.jsonl"), "a"))
        .unwrap();
    assert_eq!(resumed.source_id(), first_id);
    append(&old.join("a.jsonl"), &line("a", 3, false));
    wait("the resumed source", || {
        let state = server.state();
        (state.events.iter())
            .filter(|e| e.body.provenance.source_id == first_id)
            .count()
            == 5
    });
    assert_eq!(resumed.status(), Some(SourceStatus::Collecting));
    {
        let state = server.state();
        assert_eq!(state.calls, ["prepare", "activate", "boot"]);
        let expected: Vec<u64> = (1..=state.events.len() as u64).collect();
        assert_eq!(
            state
                .events
                .iter()
                .map(|e| e.body.sequence)
                .collect::<Vec<_>>(),
            expected,
            "no duplicate and no missing sequence across the restart"
        );
        // The lost-receipt batch was sent twice, byte for byte; every other once.
        let mut sent: BTreeMap<&[u8], usize> = BTreeMap::new();
        for post in &state.posts {
            *sent.entry(post.as_slice()).or_default() += 1;
        }
        let lost = &state.posts[1];
        assert_eq!(sent[lost.as_slice()], 2);
        assert_eq!(sent.values().filter(|n| **n != 1).count(), 1);
        // Resumed, not fenced: no gap record, no event twice, generation 1.
        assert!(state.events.iter().all(|e| kind(e) != "gap"));
        let ids: BTreeSet<Uuid> = state.events.iter().map(|e| e.event_id).collect();
        assert_eq!(ids.len(), state.events.len());
        let own: Vec<String> = (state.events.iter())
            .filter(|e| e.body.provenance.source_id == first_id)
            .map(kind)
            .collect();
        assert_eq!(
            own,
            [
                "text_reference",
                "tool_started",
                "tool_result",
                "text_reference",
                "tool_started"
            ]
        );
        assert!(
            (state.events.iter()).all(|e| e.body.provenance.source_generation == 1),
            "same source, generation 1"
        );
    }
    // Finished and earlier-boot sources leave the store once acknowledged.
    drop((second, resumed));
    wait("release", || {
        let state = server.state();
        state.through == state.events.len() as u64 && state.through >= 7
    });
    std::thread::sleep(Duration::from_millis(100));
    node.shutdown();
    let mut peer = server.clone();
    let root = std::fs::canonicalize(temp.path().join("observations"))
        .unwrap()
        .join(NODE.to_string());
    let mut intent = LocalIntent::open(&root, Uuid::from_u128(60)).unwrap();
    intent.resume(&mut peer).unwrap();
    intent
        .with_active(|store| {
            store.audit()?;
            ensure!(store.source_count()? == 0, "sources retained");
            let snapshot = store.snapshot()?;
            ensure!(
                snapshot.seen == 0 && snapshot.queued == 0 && snapshot.acked == 7,
                "rows retained: {snapshot:?}"
            );
            Ok(())
        })
        .unwrap();
}

/// A stored source nobody registers again after a restart is not left silent:
/// after the resume grace the server gets the explicit "fenced by worker
/// restart" record, and the source is released.
#[test]
fn a_source_nobody_resumes_is_fenced_by_worker_restart_after_the_grace() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let (node, observations, _) = start(&temp, 6, &server);
    let old = transcripts(&temp, "old");
    let first = observations
        .register(spec(4, 6, &old.join("a.jsonl"), "a"))
        .unwrap();
    append(&old.join("a.jsonl"), &line("a", 1, false));
    wait("its events", || server.state().events.len() == 2);
    node.shutdown();
    let id = first.source_id();
    drop(first);
    server.state().boot = Uuid::from_u128(60);
    let limits = NodeLimits {
        resume_grace: Duration::from_millis(50),
        ..NodeLimits::default()
    };
    let (node, observations, _) = start_with(&temp, 60, &server, limits);
    wait("the fence record", || server.state().events.len() == 3);
    {
        let state = server.state();
        let fence = &state.events[2];
        assert_eq!((kind(fence).as_str(), fence.body.sequence), ("gap", 3));
        assert_eq!(fence.body.provenance.source_id, id);
        assert_eq!(
            fence.body.provenance.parser.id.as_str(),
            a::CONTROL_PARSERS[2]
        );
        assert_eq!(a::control_record(fence), Some(true));
        let SourceCursor::Record { id: cursor } = &fence.body.provenance.cursor else {
            panic!("fence cursor")
        };
        assert!(cursor.as_str().starts_with(a::FENCED_BY_RESTART));
    }
    // Too late to resume: the source is held, never re-read from the start.
    let late = observations
        .register(spec(4, 6, &old.join("a.jsonl"), "a"))
        .unwrap();
    wait("the late registration refused", || {
        matches!(late.status(), Some(SourceStatus::Incomplete(_)))
    });
    assert_eq!(sequences(&server), [1, 2, 3]);
    node.shutdown();
}

/// The server refuses one source's records (here: every record of it). The
/// batch still lands, the sequence advances, the other source keeps flowing
/// and the refused one is marked incomplete with the reason.
#[test]
fn a_source_the_server_refuses_is_incomplete_and_the_others_keep_their_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let (node, observations, _) = start(&temp, 6, &server);
    let (good, bad) = (transcripts(&temp, "good"), transcripts(&temp, "bad"));
    let refused = observations
        .register(spec(12, 6, &bad.join("b.jsonl"), "b"))
        .unwrap();
    let healthy = observations
        .register(spec(4, 6, &good.join("g.jsonl"), "g"))
        .unwrap();
    server.state().refuse.insert(refused.source_id());
    append(&bad.join("b.jsonl"), &line("b", 1, false));
    append(&good.join("g.jsonl"), &line("g", 1, false));
    wait(
        "the refusal reaches the source",
        || matches!(refused.status(), Some(SourceStatus::Incomplete(reason)) if reason.contains("refused by the server")),
    );
    append(&good.join("g.jsonl"), &line("g", 2, true));
    wait("the healthy source", || server.state().events.len() == 3);
    let state = server.state();
    assert!((state.events.iter()).all(|e| e.body.provenance.source_id == healthy.source_id()));
    // Four records were sequenced (two refused); nothing waits behind them.
    assert_eq!(state.through, 5);
    assert_eq!(healthy.status(), Some(SourceStatus::Collecting));
    drop(state);
    node.shutdown();
}

/// A session far longer than the remembered window keeps being collected:
/// old identities are forgotten, nothing is sent twice, nothing is incomplete.
#[test]
fn a_session_longer_than_the_identity_window_is_collected_to_its_end() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let limits = NodeLimits {
        tracked_identities: 16,
        ..NodeLimits::default()
    };
    let (node, observations, _) = start_with(&temp, 6, &server, limits);
    let directory = transcripts(&temp, "long");
    let path = directory.join("s.jsonl");
    let handle = observations.register(spec(4, 6, &path, "s")).unwrap();
    // 200 lines, three facts per pair: 300 identities through a window of 16.
    for n in (1..200).step_by(2) {
        append(&path, &line("s", n, false));
        append(&path, &line("s", n + 1, true));
    }
    wait("the whole session", || server.state().events.len() == 300);
    assert_eq!(handle.status(), Some(SourceStatus::Collecting));
    {
        let state = server.state();
        assert_eq!(server_snapshot(&state), (1..=300).collect::<Vec<u64>>());
        let ids: BTreeSet<Uuid> = state.events.iter().map(|e| e.event_id).collect();
        assert_eq!(ids.len(), 300);
        assert!(state.events.iter().all(|e| kind(e) != "gap"));
    }
    node.shutdown();
    let mut peer = server.clone();
    let root = std::fs::canonicalize(temp.path().join("observations"))
        .unwrap()
        .join(NODE.to_string());
    let mut intent = LocalIntent::open(&root, Uuid::from_u128(6)).unwrap();
    intent.resume(&mut peer).unwrap();
    intent
        .with_active(|store| {
            store.audit()?;
            ensure!(store.snapshot()?.seen == 16, "the window is the last 16");
            Ok(())
        })
        .unwrap();
}
fn server_snapshot(state: &State) -> Vec<u64> {
    state.events.iter().map(|e| e.body.sequence).collect()
}

/// Every worker restart is one boot advance. Nothing counts them down: the
/// 70th restart delivers like the first, and a process of a boot the server
/// no longer admits still cannot take the owner or write.
#[test]
fn a_node_restarts_past_64_boots_and_a_stale_boot_still_cannot_write() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(1000);
    let directory = transcripts(&temp, "t");
    for boot in 1000..1070u128 {
        server.state().boot = Uuid::from_u128(boot);
        let (node, _, _) = start(&temp, boot, &server);
        wait("the owner on this boot", || {
            (server.state().owner.as_ref())
                .is_some_and(|o| o.current_boot_id == Uuid::from_u128(boot))
        });
        node.shutdown();
    }
    assert_eq!(server.state().owner.as_ref().unwrap().boot_transitions, 69);
    // A stale process (boot 1000) while the server admits boot 1070.
    server.state().boot = Uuid::from_u128(1070);
    let (stale, observations, log) = start(&temp, 1000, &server);
    let path = directory.join("s.jsonl");
    let _handle = observations.register(spec(4, 1000, &path, "s")).unwrap();
    append(&path, &line("s", 1, false));
    wait("the stale boot refused", || {
        (log.lock().unwrap().iter()).any(|l| l.contains("not active"))
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(server.state().events.is_empty());
    assert_eq!(server.state().owner.as_ref().unwrap().boot_transitions, 69);
    stale.shutdown();
    // The admitted boot takes over and delivers.
    let (node, observations, _) = start(&temp, 1070, &server);
    let handle = observations.register(spec(9, 1070, &path, "s")).unwrap();
    wait("delivery on the 71st boot", || {
        server.state().events.len() == 2
    });
    assert_eq!(server.state().owner.as_ref().unwrap().boot_transitions, 70);
    drop(handle);
    node.shutdown();
    // The store kept a rolling boot history and still audits clean.
    let mut peer = server.clone();
    let root = std::fs::canonicalize(temp.path().join("observations"))
        .unwrap()
        .join(NODE.to_string());
    let mut intent = LocalIntent::open(&root, Uuid::from_u128(1070)).unwrap();
    intent.resume(&mut peer).unwrap();
    intent.with_active(|store| store.audit()).unwrap();
}
