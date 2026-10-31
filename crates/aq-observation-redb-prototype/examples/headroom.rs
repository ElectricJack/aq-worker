//! Synthetic managed-owner capacity experiment. Never deletes retained Seen or
//! event-ID history, changes limits, compacts, or contacts a provider/service.
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
use anyhow::{Context, Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation::{source::SourceFile, spool::OverflowPolicy};
use aq_observation_admission::{self as admission, Owner, Reply, Request, Status};
use aq_observation_engine as engine;
use aq_observation_redb_prototype::{AuthenticatedOwnerPeer, Limits, LocalIntent};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Instant,
};
use uuid::Uuid;
#[derive(Default)]
struct Coordinator {
    owner: Option<Owner>,
    ownership: BTreeMap<Uuid, admission::Receipt>,
    receipts: BTreeMap<Uuid, Receipt>,
    through: u64,
    node_boot: u128,
}
impl AuthenticatedOwnerPeer for Coordinator {
    fn mutate(&mut self, r: &Request) -> Result<Reply> {
        let change = admission::apply(
            self.owner.as_ref(),
            self.ownership.get(&r.operation_id),
            r,
            &admission::Access {
                node_id: Uuid::from_u128(5),
                boot_id: Uuid::from_u128(if self.node_boot == 0 {
                    6
                } else {
                    self.node_boot
                }),
                accepted_through: self.through,
                max_pending_distance: admission::MAX_PENDING_DISTANCE,
                new_boot_previously_owned: false,
            },
            Some(Uuid::from_u128(400)),
        )?;
        self.owner = Some(change.reply.current_owner.clone());
        self.ownership
            .insert(r.operation_id, change.reply.receipt.clone());
        Ok(change.reply)
    }
    fn status(&mut self, node: Uuid) -> Result<Status> {
        Ok(Status {
            version: 1,
            node_id: node,
            admitted_node_boot_id: Uuid::from_u128(if self.node_boot == 0 {
                6
            } else {
                self.node_boot
            }),
            owner: self.owner.clone(),
            accepted_through: self.through,
        })
    }
}
impl Coordinator {
    fn receive(&mut self, p: &Packet) -> Result<Receipt> {
        p.validate()?;
        if let Some(r) = self.receipts.get(&p.body.message_id) {
            r.validate_against(p)?;
            return Ok(r.clone());
        }
        let Message::Batch(b) = &p.body.message else {
            anyhow::bail!("batch required")
        };
        ensure!(
            b.first_sequence == self.through + 1
                && b.last_sequence <= 65536
                && self.receipts.len() < 4096,
            "bounded fake coordinator prefix"
        );
        let r = support::receipt(p);
        self.through = b.last_sequence;
        self.receipts.insert(p.body.message_id, r.clone());
        Ok(r)
    }
}
struct Input {
    id: Uuid,
    file: SourceFile,
    path: PathBuf,
    key: String,
}
struct Pending {
    packet: Packet,
    receipt: Receipt,
}
fn disk(path: &Path) -> Value {
    let m = std::fs::metadata(path).unwrap();
    json!({"file_bytes":m.len(),"allocated_bytes":m.blocks()*512})
}
fn append(path: &Path, b: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(b).unwrap();
    f.sync_all().unwrap();
}
fn bytes(input: &Input, call: u64, fat: bool) -> Vec<u8> {
    let mut b = vec![];
    for n in 0..32 {
        let mut v: Value =
            serde_json::from_slice(&support::row(if fat { call } else { call + n })).unwrap();
        v["sessionId"] = json!(input.key);
        if fat {
            v["synthetic_capture_hint"] = json!(n);
        }
        b.extend(serde_json::to_vec(&v).unwrap());
        b.push(b'\n');
    }
    b
}
fn source(i: u128) -> engine::Source {
    let mut v = serde_json::to_value(support::source(8 + i)).unwrap();
    v["native_session_key"] = json!(format!("native-fixture-{i}"));
    v["binding"]["task_id"] = json!(Uuid::from_u128(2000 + i));
    v["binding"]["attempt_id"] = json!(Uuid::from_u128(4000 + i));
    v["binding"]["session"]["attempt_id"] = json!(Uuid::from_u128(4000 + i));
    v["binding"]["session"]["session_id"] = json!(Uuid::from_u128(7000 + i));
    serde_json::from_value(v).unwrap()
}
fn fingerprint(owner: &LocalIntent) -> Result<String> {
    owner.with_active(|s| s.audit_fingerprint())
}
fn sample<T>(
    owner: &LocalIntent,
    db: &Path,
    stage: &str,
    rows: &mut Vec<Value>,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let monitor = owner.store_monitor()?;
    monitor.reset();
    let before = disk(db);
    let now = Instant::now();
    let result = f();
    let after = disk(db);
    rows.push(json!({"stage":stage,"ms":now.elapsed().as_secs_f64()*1000.,"io":monitor.stats(),"before":before,"after":after,"error":result.as_ref().err().map(|e|format!("{e:#}"))}));
    ensure!(rows.len() <= 50000, "bounded measurement samples");
    result
}
fn arm(
    owner: &LocalIntent,
    peer: &mut Coordinator,
    pending: &mut Option<Pending>,
    time: &mut u64,
    db: &Path,
    rows: &mut Vec<Value>,
) -> Result<()> {
    if pending.is_some() {
        return Ok(());
    }
    let packet = sample(owner, db, "prepare", rows, || {
        owner.with_active(|s| s.prepare())
    })?;
    if let Some(packet) = packet {
        *time += 1000;
        let sent = sample(owner, db, "send", rows, || {
            owner.with_active(|s| s.begin_send(*time))
        })?
        .context("pending send absent")?;
        ensure!(sent == packet, "send changed frozen packet");
        let receipt = peer.receive(&sent)?;
        *pending = Some(Pending { packet, receipt });
    }
    Ok(())
}
fn ack(
    owner: &LocalIntent,
    pending: &mut Option<Pending>,
    db: &Path,
    rows: &mut Vec<Value>,
) -> Result<()> {
    if let Some(p) = pending {
        sample(owner, db, "ack", rows, || {
            owner.with_active(|s| s.acknowledge(&p.receipt))
        })?;
        *pending = None;
    }
    Ok(())
}
fn reopen(owner: &mut Option<LocalIntent>, root: &Path, peer: &mut Coordinator) -> Result<()> {
    drop(owner.take());
    let mut next = LocalIntent::open(root, Uuid::from_u128(6))?;
    next.resume(peer)?;
    *owner = Some(next);
    Ok(())
}
fn recover_pending(
    owner: &LocalIntent,
    peer: &mut Coordinator,
    pending: &mut Option<Pending>,
    time: &mut u64,
) -> Result<Value> {
    let before = owner.with_active(|s| s.snapshot())?;
    let mut replayed = false;
    let mut already = false;
    if let Some(p) = pending {
        if before.acked >= p.receipt.through_sequence.context("batch receipt cursor")? {
            ensure!(
                before.receipt.as_ref() == Some(&p.receipt),
                "uncertain ACK lacks exact retained receipt"
            );
            owner.with_active(|s| s.acknowledge(&p.receipt))?;
            already = true;
        } else {
            let frozen = owner
                .with_active(|s| s.prepare())?
                .context("frozen packet lost")?;
            ensure!(frozen == p.packet, "reopen changed packet bytes");
            *time += 1000;
            let packet = owner
                .with_active(|s| s.begin_send(*time))?
                .context("replay absent")?;
            ensure!(
                packet == p.packet && peer.receive(&packet)? == p.receipt,
                "exact replay/receipt changed"
            );
            owner.with_active(|s| s.acknowledge(&p.receipt))?;
            replayed = true;
        }
        *pending = None;
    } else if before.delivery.is_some() {
        let packet = owner.with_active(|s| s.prepare())?.unwrap();
        *time += 1000;
        let sent = owner
            .with_active(|s| s.begin_send(*time))?
            .context("unknown send replay absent")?;
        ensure!(packet == sent, "unknown frozen packet changed");
        let r = peer.receive(&sent)?;
        owner.with_active(|s| s.acknowledge(&r))?;
        replayed = true;
    }
    Ok(
        json!({"replayed_exact_packet_and_receipt":replayed,"already_committed_ack":already,"after":owner.with_active(|s|s.snapshot())?}),
    )
}
#[allow(clippy::too_many_arguments)] // Explicit fault-harness state, not a runtime API.
fn failure_recovery(
    owner: &mut Option<LocalIntent>,
    root: &Path,
    db: &Path,
    peer: &mut Coordinator,
    pending: &mut Option<Pending>,
    time: &mut u64,
    error: &anyhow::Error,
    phase: &str,
    before: Option<&str>,
) -> Value {
    let prior = disk(db);
    match reopen(owner, root, peer) {
        Err(e) => {
            json!({"phase":phase,"error":format!("{error:#}"),"reopen_error":format!("{e:#}"),"disk_before_reopen":prior,"disk_after_reopen":disk(db)})
        }
        Ok(()) => {
            let fp = fingerprint(owner.as_ref().unwrap());
            let recovered = recover_pending(owner.as_ref().unwrap(), peer, pending, time);
            json!({"phase":phase,"error":format!("{error:#}"),"before_failure_projection":before,"after_reopen_projection":fp.as_ref().ok(),"fingerprint_error":fp.as_ref().err().map(|e|format!("{e:#}")),"unchanged_projection":before.and_then(|b|fp.as_ref().ok().map(|f|b==f)),"disk_before_reopen":prior,"disk_after_reopen":disk(db),"pending_recovery":recovered.as_ref().ok(),"pending_recovery_error":recovered.err().map(|e|format!("{e:#}"))})
        }
    }
}
fn run(fat: bool) -> Result<Value> {
    run_configured(fat, Limits::fixture(), false)
}
fn run_configured(fat: bool, limits: Limits, maximal_keys: bool) -> Result<Value> {
    let temp = tempfile::tempdir()?;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
    let root = temp.path().join("owner");
    let db = root.join("store/store.redb");
    let mut peer = Coordinator::default();
    let mut local = LocalIntent::create(
        &root,
        limits.clone(),
        Uuid::from_u128(401),
        Uuid::from_u128(402),
    )?;
    local.prepare(&mut peer)?;
    local.create_store(&mut peer)?;
    local.activate(&mut peer, Uuid::from_u128(403))?;
    let mut owner = Some(local);
    let mut inputs = vec![];
    for i in 0..16 {
        let mut s = source(i);
        if maximal_keys {
            s.native_session_key = Text::new("\"".repeat(512))?;
        }
        let raw = temp.path().join(format!("raw-{i}"));
        std::fs::DirBuilder::new().mode(0o700).create(&raw)?;
        let path = raw.join("native.jsonl");
        std::fs::write(&path, [])?;
        let permit = SourceFile::inspect(
            s.source_id,
            &raw,
            "native.jsonl",
            s.native_session_key.clone(),
        )?;
        let file = SourceFile::open(permit.clone())?;
        owner
            .as_ref()
            .unwrap()
            .with_active(|store| store.attach_file_engine(s.clone(), 4096, permit))?;
        inputs.push(Input {
            id: s.source_id,
            file,
            path,
            key: s.native_session_key.as_str().into(),
        });
    }
    let initial = owner.as_ref().unwrap().with_active(|s| s.snapshot())?;
    let mut rows = vec![];
    let mut pending = None;
    let mut time = 0;
    let mut cycle = 0u64;
    let mut current = 0usize;
    let mut raw_pending = false;
    let mut near = None;
    let mut refusals = vec![];
    let mut recovery = None;
    let mut status = "bounded_work_limit_without_refusal";
    let mut error_recovery = None;
    macro_rules! capacity_try {
        ($label:lifetime,$value:expr,$phase:expr) => {
            match $value {
                Ok(value) => value,
                Err(error) => {
                    status = "operation_error";
                    error_recovery = Some(failure_recovery(
                        &mut owner,
                        &root,
                        &db,
                        &mut peer,
                        &mut pending,
                        &mut time,
                        &error,
                        $phase,
                        None,
                    ));
                    break $label;
                }
            }
        };
    }
    'work: while cycle < if fat { 8192 } else { 2048 } {
        let o = owner.as_ref().unwrap();
        let view = o.with_active(|s| s.snapshot())?;
        let near_capacity = view.logical_bytes
            >= limits.ordinary_limit().saturating_sub(128 * 1024)
            || std::fs::metadata(&db)?.len() >= 44 * 1024 * 1024;
        let before_fingerprint = if near_capacity {
            Some(fingerprint(o)?)
        } else {
            None
        };
        if near.is_none() && near_capacity {
            let fp = before_fingerprint.clone().unwrap();
            let disk_before = disk(&db);
            capacity_try!('work,reopen(&mut owner,&root,&mut peer),"near/refusal reopen");
            ensure!(
                fingerprint(owner.as_ref().unwrap())? == fp,
                "near-capacity reopen changed full projection"
            );
            near = Some(
                json!({"snapshot":view,"projection_sha256":fp,"before":disk_before,"after_reopen":disk(&db)}),
            );
        }
        if !raw_pending {
            current = if fat {
                (cycle / 1024) as usize
            } else {
                (cycle * 32 / 4096) as usize
            };
            let call = if fat { cycle % 1024 } else { cycle * 32 % 4096 };
            append(
                &inputs[current].path,
                &bytes(&inputs[current], current as u64 * 1_000_000 + call, fat),
            );
            cycle += 1;
        }
        let input = &inputs[current];
        let o = owner.as_ref().unwrap();
        let before = o.with_active(|s| s.source(input.id))?;
        let collected = sample(o, &db, "collect32", &mut rows, || {
            o.with_active(|s| s.collect_file(input.id, &input.file, time + 1, OverflowPolicy::Hold))
        });
        let result = match collected {
            Ok(r) => r,
            Err(e) => {
                status = "physical_or_transaction_error";
                error_recovery = Some(failure_recovery(
                    &mut owner,
                    &root,
                    &db,
                    &mut peer,
                    &mut pending,
                    &mut time,
                    &e,
                    "collect32",
                    before_fingerprint.as_deref(),
                ));
                break;
            }
        };
        let after = owner
            .as_ref()
            .unwrap()
            .with_active(|s| s.source(input.id))?;
        ensure!(
            after.cursor == before.cursor + result.consumed_bytes,
            "cursor not matching committed prefix"
        );
        raw_pending = after.cursor < std::fs::metadata(&input.path)?.len();
        let view = owner.as_ref().unwrap().with_active(|s| s.snapshot())?;
        if result.blocked {
            refusals.push(json!({"cycle":cycle,"source_id":input.id,"consumed_bytes":result.consumed_bytes,"facts_added":result.facts_added,"snapshot":view,"disk":disk(&db)}));
            if recovery.is_none() {
                let fp = fingerprint(owner.as_ref().unwrap())?;
                capacity_try!('work,reopen(&mut owner,&root,&mut peer),"near/refusal reopen");
                ensure!(
                    fingerprint(owner.as_ref().unwrap())? == fp,
                    "logical-refusal reopen changed projection"
                );
                let p = capacity_try!('work,
                    recover_pending(owner.as_ref().unwrap(), &mut peer, &mut pending, &mut time),
                    "logical-refusal replay/ACK"
                );
                recovery = Some(json!({"projection_sha256":fp,"replay":p,"disk_after":disk(&db)}));
            }
            while owner
                .as_ref()
                .unwrap()
                .with_active(|s| Ok(s.snapshot()?.queued))?
                > 0
            {
                capacity_try!('work,
                    arm(
                        owner.as_ref().unwrap(),
                        &mut peer,
                        &mut pending,
                        &mut time,
                        &db,
                        &mut rows,
                    ),
                    "arm"
                );
                capacity_try!('work,
                    ack(owner.as_ref().unwrap(), &mut pending, &db, &mut rows),
                    "ack"
                );
            }
            if result.consumed_bytes == 0 {
                let o = owner.as_ref().unwrap();
                let fp = fingerprint(o)?;
                let retry = capacity_try!('work, sample(o, &db, "stable-refusal-retry", &mut rows, || {
                    o.with_active(|s| {
                        s.collect_file(input.id, &input.file, time + 1, OverflowPolicy::Hold)
                    })
                }),"stable-refusal-retry");
                if retry.blocked && retry.consumed_bytes == 0 {
                    ensure!(
                        fingerprint(o)? == fp,
                        "stable refusal modified retained history"
                    );
                    status = "logical_refusal_reopened_replayed_and_drained";
                    break;
                }
            }
        } else {
            let queued = view.queued;
            if queued >= 32 {
                capacity_try!('work,
                    arm(
                        owner.as_ref().unwrap(),
                        &mut peer,
                        &mut pending,
                        &mut time,
                        &db,
                        &mut rows,
                    ),
                    "arm"
                );
            }
            if queued >= 64 {
                capacity_try!('work,
                    ack(owner.as_ref().unwrap(), &mut pending, &db, &mut rows),
                    "ack"
                );
                capacity_try!('work,
                    arm(
                        owner.as_ref().unwrap(),
                        &mut peer,
                        &mut pending,
                        &mut time,
                        &db,
                        &mut rows,
                    ),
                    "arm"
                );
            }
        }
    }
    let ordinary_final = owner.as_ref().unwrap().with_active(|s| s.snapshot())?;
    let history = owner
        .as_ref()
        .unwrap()
        .with_active(|s| s.audit_retained_fingerprint(&[]))?;
    let input = &inputs[current];
    let gap = owner.as_ref().unwrap().with_active(|s| {
        s.collect_file(input.id, &input.file, time + 1, OverflowPolicy::GapAndHold)
    })?;
    ensure!(
        gap.blocked && gap.facts_added == 1,
        "reserved spool-loss gap not admitted after ordinary refusal"
    );
    ensure!(
        owner
            .as_ref()
            .unwrap()
            .with_active(|s| Ok(s.snapshot()?.seen))?
            == ordinary_final.seen,
        "control deleted/added native history"
    );
    peer.node_boot = 66;
    drop(owner.take());
    let mut next = LocalIntent::open(&root, Uuid::from_u128(66))?;
    next.advance_boot(
        &mut peer,
        Uuid::from_u128(404),
        Uuid::from_u128(66),
        time + 1,
    )?;
    owner = Some(next);
    let packet = owner
        .as_ref()
        .unwrap()
        .with_active(|s| s.prepare())?
        .context("reserved controls absent")?;
    let Message::Batch(batch) = &packet.body.message else {
        anyhow::bail!("control batch required")
    };
    ensure!(
        batch.events.len() == 16,
        "already-held source must not consume another boot-gap slot"
    );
    let control_ids: Vec<_> = batch.events.iter().map(|e| e.event_id).collect();
    ensure!(
        owner
            .as_ref()
            .unwrap()
            .with_active(|s| s.audit_retained_fingerprint(&control_ids))?
            == history,
        "control changed retained dedupe/event history"
    );
    let control_snapshot = owner.as_ref().unwrap().with_active(|s| s.snapshot())?;
    let receipt = peer.receive(&packet)?;
    owner
        .as_ref()
        .unwrap()
        .with_active(|s| s.acknowledge(&receipt))?;
    let history_after_control_ack = owner
        .as_ref()
        .unwrap()
        .with_active(|s| s.audit_retained_fingerprint(&[]))?;
    let sequence_after_control = owner
        .as_ref()
        .unwrap()
        .with_active(|s| Ok(s.snapshot()?.next))?;
    // Exercise the full bounded boot metadata lifetime with no live sources.
    // Repeated boots cannot allocate another gap for an already-held source.
    for index in 1..64 {
        let boot = Uuid::from_u128(66 + index);
        peer.node_boot = 66 + index;
        drop(owner.take());
        let mut next = LocalIntent::open(&root, boot)?;
        next.advance_boot(&mut peer, Uuid::from_u128(404 + index), boot, time + 1)?;
        owner = Some(next);
    }
    ensure!(
        owner
            .as_ref()
            .unwrap()
            .with_active(|s| Ok(s.snapshot()?.next))?
            == sequence_after_control,
        "repeated held-source boot fabricated events"
    );
    ensure!(
        owner
            .as_ref()
            .unwrap()
            .with_active(|s| s.audit_retained_fingerprint(&[]))?
            == history_after_control_ack,
        "repeated boot changed retained history"
    );
    let control_probe = json!({"gap_facts":gap.facts_added,"gap_consumed_bytes":gap.consumed_bytes,"boot_gap_facts":batch.events.len()-1,"total_urgent_facts":batch.events.len(),"pre_control":ordinary_final,"post_control_before_ack":control_snapshot,"retained_history_sha256":history,"history_preserved":true,"total_boot_transitions":64,"sequence_after_control":sequence_after_control,"repeated_boots_added_events":false});
    let final_view = if let Some(o) = &owner {
        Some(o.with_active(|s| s.snapshot())?)
    } else {
        None
    };
    let final_fingerprint = owner.as_ref().map(fingerprint);
    let final_fingerprint_error = final_fingerprint
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("{e:#}"));
    let final_fingerprint = final_fingerprint.and_then(Result::ok);
    let sources = owner
        .as_ref()
        .map(|o| {
            inputs
                .iter()
                .map(|i| {
                    o.with_active(|s| {
                        let r = s.source(i.id)?;
                        Ok(json!({"id":i.id,"cursor":r.cursor,"known":r.engine.map(|e|e.counts)}))
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    Ok(
        json!({"fixture":"synthetic Claude-shaped data; managed fake transport authority, real file/normalizer/redb/CON03 pipeline; no native/version/production/source-authority qualification","mode":if fat{"32_fingerprints_per_call"}else{"32_unique_calls_per_batch"},"limits":limits,"status":status,"initial":initial,"near":near,"first_refusals":refusals,"recovery":recovery,"control_probe":control_probe,"error_recovery":error_recovery,"final":final_view,"final_projection_sha256":final_fingerprint,"final_fingerprint_error":final_fingerprint_error,"final_disk":disk(&db),"sources":sources,"appended_batches":cycle,"fake_central_through":peer.through,"unique_packet_receipts":peer.receipts.len(),"raw_fixture_bytes":inputs.iter().map(|i|std::fs::metadata(&i.path).unwrap().len()).sum::<u64>(),"samples":rows,"compaction_called":false,"history_deleted":false}),
    )
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--check-old-root" {
        let root = Path::new(&args[2]);
        let db = root.join("store/store.redb");
        let intent = root.join("intent.json");
        let before = std::fs::read(&db).unwrap();
        let before_intent = std::fs::read(&intent).unwrap();
        let store =
            aq_observation_redb_prototype::Store::open(&root.join("store"), Limits::fixture());
        let owner = LocalIntent::open(root, Uuid::from_u128(66));
        assert!(store.is_err() && owner.is_err());
        assert_eq!(before, std::fs::read(&db).unwrap());
        assert_eq!(before_intent, std::fs::read(&intent).unwrap());
        use sha2::{Digest as _, Sha256};
        println!(
            "{}",
            json!({"old_root":root,"refused":true,"database_bytes_unchanged":true,"intent_bytes_unchanged":true,"database_sha256":format!("{:x}",Sha256::digest(&before)),"new_format_marker_created":root.join("store/store.format").exists()})
        );
        return;
    }
    let started = Instant::now();
    let result = (|| -> Result<Value> {
        match std::env::var("AQ_OBSERVATION_HEADROOM_MODE").as_deref() {
            Ok("fingerprints") => Ok(
                json!({"fingerprints":run(true)?,"elapsed_ms":started.elapsed().as_secs_f64()*1000.}),
            ),
            Ok("unique") => {
                Ok(json!({"unique":run(false)?,"elapsed_ms":started.elapsed().as_secs_f64()*1000.}))
            }
            _ => Ok(
                json!({"unique":run(false)?,"fingerprints":run(true)?,"elapsed_ms":started.elapsed().as_secs_f64()*1000.}),
            ),
        }
    })();
    match result {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("headroom experiment failed: {e:#}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_observation_redb_prototype::backend::{Action, Fault};
    #[test]
    fn prepare_send_and_ack_errors_keep_report_and_exact_recovery() {
        for stage in ["prepare", "send", "ack"] {
            let t = tempfile::tempdir().unwrap();
            let root = t.path().join("owner");
            let db = root.join("store/store.redb");
            let mut peer = Coordinator::default();
            let mut local = LocalIntent::create(
                &root,
                Limits::fixture(),
                Uuid::from_u128(401),
                Uuid::from_u128(402),
            )
            .unwrap();
            local.prepare(&mut peer).unwrap();
            local.create_store(&mut peer).unwrap();
            local.activate(&mut peer, Uuid::from_u128(403)).unwrap();
            local
                .with_active(|s| {
                    s.attach(support::source(8))?;
                    support::append(s, 1);
                    Ok(())
                })
                .unwrap();
            let mut owner = Some(local);
            let mut pending = None;
            let mut time = 0;
            let mut rows = vec![];
            if stage == "send" {
                owner
                    .as_ref()
                    .unwrap()
                    .with_active(|s| s.prepare())
                    .unwrap();
            }
            if stage == "ack" {
                arm(
                    owner.as_ref().unwrap(),
                    &mut peer,
                    &mut pending,
                    &mut time,
                    &db,
                    &mut rows,
                )
                .unwrap();
            }
            let fp = fingerprint(owner.as_ref().unwrap()).unwrap();
            let monitor = owner.as_ref().unwrap().store_monitor().unwrap();
            monitor.reset();
            monitor.fault(Fault {
                operation: 1,
                after: false,
                action: Action::Error,
            });
            // sample() resets instrumentation intentionally, so inject directly around
            // the operation here, then exercise the same common recovery reporter.
            let error = match stage {
                "prepare" => owner
                    .as_ref()
                    .unwrap()
                    .with_active(|s| s.prepare().map(|_| ()))
                    .unwrap_err(),
                "send" => owner
                    .as_ref()
                    .unwrap()
                    .with_active(|s| s.begin_send(1000).map(|_| ()))
                    .unwrap_err(),
                _ => owner
                    .as_ref()
                    .unwrap()
                    .with_active(|s| s.acknowledge(&pending.as_ref().unwrap().receipt))
                    .unwrap_err(),
            };
            let report = failure_recovery(
                &mut owner,
                &root,
                &db,
                &mut peer,
                &mut pending,
                &mut time,
                &error,
                stage,
                Some(&fp),
            );
            assert_eq!(report["unchanged_projection"], true);
            assert!(report["pending_recovery_error"].is_null());
            let projection = owner
                .as_ref()
                .unwrap()
                .with_active(|s| s.snapshot())
                .unwrap();
            assert_eq!(projection.seen, 1);
            assert_eq!(projection.next, 2);
            if stage != "prepare" {
                assert_eq!(projection.acked, 1);
            } else {
                arm(
                    owner.as_ref().unwrap(),
                    &mut peer,
                    &mut pending,
                    &mut time,
                    &db,
                    &mut rows,
                )
                .unwrap();
                ack(owner.as_ref().unwrap(), &mut pending, &db, &mut rows).unwrap();
            }
        }
    }
    #[test]
    fn full_ordinary_budget_preserves_all16_control_credits_and64_boots() {
        let mut limits = Limits::fixture();
        limits.logical_bytes = limits.logical_bytes - limits.ordinary_limit() + 256 * 1024;
        let report = run_configured(true, limits, true).unwrap();
        assert_eq!(
            report["status"],
            "logical_refusal_reopened_replayed_and_drained"
        );
        assert_eq!(report["control_probe"]["total_urgent_facts"], 16);
        assert_eq!(report["control_probe"]["boot_gap_facts"], 15);
        assert_eq!(report["control_probe"]["history_preserved"], true);
        assert_eq!(report["control_probe"]["total_boot_transitions"], 64);
        assert_eq!(report["final"]["queued"], 0);
        assert_eq!(
            report["final"]["seen"],
            report["control_probe"]["pre_control"]["seen"]
        );
    }
}
