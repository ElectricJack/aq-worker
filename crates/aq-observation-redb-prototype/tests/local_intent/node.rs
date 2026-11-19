//! The node observation service against an in-memory server that applies the
//! real transport contract and the ledger rule `first_sequence == through + 1`.
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation_admission::{self as a, Operation, Owner, Reply, Request, Status};
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
    acked: BTreeMap<Uuid, Receipt>,
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
    fn batch(&mut self, packet: &Packet) -> Result<Receipt> {
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
        let receipt = super::support::receipt(packet);
        s.through = batch.last_sequence;
        s.events.extend(batch.events.iter().cloned());
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
    let log = Log::default();
    let lines = log.clone();
    let limits = NodeLimits {
        poll: Duration::from_millis(5),
        retry_start: Duration::from_millis(5),
        retry_max: Duration::from_millis(20),
        finish_drain: Duration::from_secs(2),
        ..NodeLimits::default()
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

/// A docker-local launch: no catalog entry, no source. With one, Claude's source reads
/// `<attempt>.jsonl` in the mounted directory and Codex's the rollout Codex named itself
/// under `sessions/YYYY/MM/DD`; both carry the server-derived ID and leave the store when
/// the attempt's handle is dropped.
#[test]
fn a_launch_registers_the_derived_source_for_both_harnesses_and_releases_it() {
    use aq_admission::observation_source;
    let temp = tempfile::tempdir().unwrap();
    let server = Server::on(6);
    let (node, observations, _) = start(&temp, 6, &server);
    let modules = [entry(), observation_source::module(&entry()).unwrap().0];
    let launch = |attempt: u128, harness: &str, modules: &[ArtifactRef]| {
        let mount = transcripts(&temp, &format!("{harness}-{attempt}"));
        let binding = spec(attempt, 6, &mount, "k").binding;
        SourceSpec::of_launch(binding, modules, harness, mount, "provider", "account")
    };
    assert!(launch(20, "claude", &modules[..1]).unwrap().is_none());
    assert!(launch(21, "gemini", &modules).is_err());
    let (claude, codex) = (
        launch(4, "claude", &modules).unwrap().unwrap(),
        launch(9, "codex", &modules).unwrap().unwrap(),
    );
    assert_eq!(claude.format, Format::ClaudeV1Jsonl9a9b8e8);
    assert_eq!(codex.format, Format::CodexRollout67f0994);
    let session = Uuid::from_u128(4).to_string();
    append(
        &claude.transcript.join(format!("{session}.jsonl")),
        &line(&session, 1, false),
    );
    // Another session's rollout comes later; a link and a stray file are never followed.
    let rollout = "0199a213-81c0-7800-8aa1-bbab2a035a53";
    let day = codex.transcript.join("2026/11/18");
    std::fs::create_dir_all(&day).unwrap();
    std::os::unix::fs::symlink(temp.path(), codex.transcript.join("2025")).unwrap();
    append(&day.join("notes.jsonl"), b"{}\n");
    for (time, id) in [("10-00-00", rollout), ("11-00-00", &session)] {
        let meta = json!({"type":"session_meta","payload":{"id":id}}).to_string() + "\n";
        let name = format!("rollout-2026-11-18T{time}-{id}.jsonl");
        append(&day.join(name), meta.as_bytes());
    }
    let again = [claude.clone(), codex.clone()];
    let handles = [claude, codex].map(|spec| observations.register(spec).unwrap());
    for (handle, attempt) in handles.iter().zip([4, 9]) {
        let derived = observation_source::source_id(Uuid::from_u128(attempt), 1, &entry());
        assert_eq!(handle.source_id(), derived.unwrap());
        wait("the launch's transcript collected", || {
            handle.status() == Some(SourceStatus::Collecting)
        });
    }
    wait("Claude's events", || server.state().events.len() == 2);
    assert!(observations.register(again[1].clone()).is_err(), "held");
    drop(handles);
    // A source's ID is free again only once the store has released it.
    for spec in again {
        wait("the source released at the attempt's end", || {
            observations.register(spec.clone()).is_ok()
        });
    }
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
fn a_restart_resends_only_the_unacknowledged_batch_and_advances_the_boot() {
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
    // A session of the ended boot is refused, not re-read from the start.
    let stale = observations
        .register(spec(4, 6, &old.join("a.jsonl"), "a"))
        .unwrap();
    wait("the stale source refused", || {
        matches!(stale.status(), Some(SourceStatus::Incomplete(_)))
    });
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
        // The interrupted source ends with an explicit gap, after its events.
        let gap = (state.events.iter())
            .position(|e| kind(e) == "gap")
            .expect("boot fence gap");
        assert_eq!(state.events[gap].body.sequence, 4);
        assert_eq!(
            state.events[gap].body.binding.attempt_id,
            Uuid::from_u128(4)
        );
    }
    // Finished and earlier-boot sources leave the store once acknowledged.
    drop((second, stale));
    wait("release", || {
        let state = server.state();
        state.through == state.events.len() as u64 && state.through >= 6
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
                snapshot.seen == 0 && snapshot.queued == 0 && snapshot.acked == 6,
                "rows retained: {snapshot:?}"
            );
            Ok(())
        })
        .unwrap();
}
