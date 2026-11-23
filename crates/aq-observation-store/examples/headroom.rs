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
use aq_observation_store::{AuthenticatedOwnerPeer, Limits, LocalIntent};
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
struct Samples {
    db: PathBuf,
    rows: Vec<Value>,
}
impl Samples {
    fn new(db: PathBuf) -> Self {
        Self { db, rows: vec![] }
    }
    fn measure<T>(
        &mut self,
        owner: &LocalIntent,
        stage: &str,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let monitor = owner.store_monitor()?;
        monitor.reset();
        let before = disk(&self.db);
        let now = Instant::now();
        let result = f();
        let after = disk(&self.db);
        self.rows.push(json!({"stage":stage,"ms":now.elapsed().as_secs_f64()*1000.,"io":monitor.stats(),"before":before,"after":after,"error":result.as_ref().err().map(|e|format!("{e:#}"))}));
        ensure!(self.rows.len() <= 50000, "bounded measurement samples");
        result
    }
}
#[derive(Default)]
struct Delivery {
    peer: Coordinator,
    pending: Option<Pending>,
    time: u64,
}
impl Delivery {
    fn arm(&mut self, owner: &LocalIntent, samples: &mut Samples) -> Result<()> {
        let Self {
            peer,
            pending,
            time,
        } = self;
        if pending.is_some() {
            return Ok(());
        }
        let packet = samples.measure(owner, "prepare", || owner.with_active(|s| s.prepare()))?;
        if let Some(packet) = packet {
            *time += 1000;
            let sent = samples
                .measure(owner, "send", || owner.with_active(|s| s.begin_send(*time)))?
                .context("pending send absent")?;
            ensure!(sent == packet, "send changed frozen packet");
            let receipt = peer.receive(&sent)?;
            *pending = Some(Pending { packet, receipt });
        }
        Ok(())
    }

    fn ack(&mut self, owner: &LocalIntent, samples: &mut Samples) -> Result<()> {
        if let Some(p) = &self.pending {
            samples.measure(owner, "ack", || {
                owner.with_active(|s| s.acknowledge(&p.receipt))
            })?;
            self.pending = None;
        }
        Ok(())
    }

    fn recover_pending(&mut self, owner: &LocalIntent) -> Result<Value> {
        let Self {
            peer,
            pending,
            time,
        } = self;
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
}
struct Experiment {
    near_logical_bytes: u64,
    owner: Option<LocalIntent>,
    root: PathBuf,
    samples: Samples,
    delivery: Delivery,
}
impl Experiment {
    fn open(root: PathBuf, limits: Limits) -> Result<Self> {
        let near_logical_bytes = limits.ordinary_limit().saturating_sub(128 * 1024);
        let mut delivery = Delivery::default();
        let mut local =
            LocalIntent::create(&root, limits, Uuid::from_u128(401), Uuid::from_u128(402))?;
        local.prepare(&mut delivery.peer)?;
        local.create_store(&mut delivery.peer)?;
        local.activate(&mut delivery.peer, Uuid::from_u128(403))?;
        let samples = Samples::new(root.join("store/store.redb"));
        Ok(Self {
            near_logical_bytes,
            owner: Some(local),
            root,
            samples,
            delivery,
        })
    }
    fn active(&self) -> &LocalIntent {
        self.owner.as_ref().expect("owner open")
    }
    fn reopen(&mut self) -> Result<()> {
        drop(self.owner.take());
        let mut owner = LocalIntent::open(&self.root, Uuid::from_u128(6))?;
        owner.resume(&mut self.delivery.peer)?;
        self.owner = Some(owner);
        Ok(())
    }
    fn failure_recovery(
        &mut self,
        error: &anyhow::Error,
        phase: &str,
        before: Option<&str>,
    ) -> Value {
        let prior = disk(&self.samples.db);
        match self.reopen() {
            Err(e) => {
                json!({"phase":phase,"error":format!("{error:#}"),"reopen_error":format!("{e:#}"),"disk_before_reopen":prior,"disk_after_reopen":disk(&self.samples.db)})
            }
            Ok(()) => {
                let fp = fingerprint(self.active());
                let recovered = self.delivery.recover_pending(self.owner.as_ref().unwrap());
                json!({"phase":phase,"error":format!("{error:#}"),"before_failure_projection":before,"after_reopen_projection":fp.as_ref().ok(),"fingerprint_error":fp.as_ref().err().map(|e|format!("{e:#}")),"unchanged_projection":before.and_then(|b|fp.as_ref().ok().map(|f|b==f)),"disk_before_reopen":prior,"disk_after_reopen":disk(&self.samples.db),"pending_recovery":recovered.as_ref().ok(),"pending_recovery_error":recovered.err().map(|e|format!("{e:#}"))})
            }
        }
    }
}
#[derive(Default)]
struct Progress {
    cycle: u64,
    current: usize,
    raw_pending: bool,
    near: Option<Value>,
    refusals: Vec<Value>,
    recovery: Option<Value>,
    error_recovery: Option<Value>,
}
struct OperationFailure {
    phase: &'static str,
    error: anyhow::Error,
}
type OperationResult<T> = std::result::Result<T, OperationFailure>;
fn operation<T>(phase: &'static str, result: Result<T>) -> OperationResult<T> {
    result.map_err(|error| OperationFailure { phase, error })
}
impl Experiment {
    fn arm(&mut self) -> OperationResult<()> {
        operation(
            "arm",
            self.delivery
                .arm(self.owner.as_ref().unwrap(), &mut self.samples),
        )
    }
    fn ack(&mut self) -> OperationResult<()> {
        operation(
            "ack",
            self.delivery
                .ack(self.owner.as_ref().unwrap(), &mut self.samples),
        )
    }
    fn collect(
        &mut self,
        input: &Input,
        stage: &str,
    ) -> Result<aq_observation_store::CollectedFrame> {
        let owner = self.owner.as_ref().unwrap();
        let now = self.delivery.time + 1;
        self.samples.measure(owner, stage, || {
            owner.with_active(|s| s.collect_file(input.id, &input.file, now, OverflowPolicy::Hold))
        })
    }
    fn send_queued(&mut self, queued: u64) -> OperationResult<()> {
        if queued >= 32 {
            self.arm()?;
        }
        if queued >= 64 {
            self.ack()?;
            self.arm()?;
        }
        Ok(())
    }
}
impl Progress {
    fn next_input<'a>(&mut self, inputs: &'a [Input], fat: bool) -> &'a Input {
        if !self.raw_pending {
            let (records_per_batch, calls_per_source) = if fat { (1, 1024) } else { (32, 4096) };
            let call = self.cycle * records_per_batch;
            self.current = (call / calls_per_source) as usize;
            let input = &inputs[self.current];
            append(
                &input.path,
                &bytes(
                    input,
                    self.current as u64 * 1_000_000 + call % calls_per_source,
                    fat,
                ),
            );
            self.cycle += 1;
        }
        &inputs[self.current]
    }
}
macro_rules! measured_operation {
    ($work:expr,$experiment:expr,$progress:expr) => {
        match $work {
            Ok(value) => value,
            Err(failure) => {
                $progress.error_recovery =
                    Some($experiment.failure_recovery(&failure.error, failure.phase, None));
                return Ok(Some("operation_error"));
            }
        }
    };
}
impl Experiment {
    fn step(
        &mut self,
        inputs: &[Input],
        progress: &mut Progress,
        fat: bool,
    ) -> Result<Option<&'static str>> {
        let view = self.active().with_active(|s| s.snapshot())?;
        let near_capacity = view.logical_bytes >= self.near_logical_bytes
            || std::fs::metadata(&self.samples.db)?.len() >= 44 * 1024 * 1024;
        let before_fingerprint = near_capacity
            .then(|| fingerprint(self.active()))
            .transpose()?;
        if let Some(fp) = before_fingerprint
            .as_ref()
            .filter(|_| progress.near.is_none())
        {
            let disk_before = disk(&self.samples.db);
            measured_operation!(
                operation("near/refusal reopen", self.reopen()),
                self,
                progress
            );
            ensure!(
                fingerprint(self.active())? == *fp,
                "near-capacity reopen changed full projection"
            );
            progress.near = Some(
                json!({"snapshot":view,"projection_sha256":fp,"before":disk_before,"after_reopen":disk(&self.samples.db)}),
            );
        }
        let input = progress.next_input(inputs, fat);
        let before = self.active().with_active(|s| s.source(input.id))?;
        let result = match self.collect(input, "collect32") {
            Ok(result) => result,
            Err(error) => {
                progress.error_recovery =
                    Some(self.failure_recovery(&error, "collect32", before_fingerprint.as_deref()));
                return Ok(Some("physical_or_transaction_error"));
            }
        };
        let after = self.active().with_active(|s| s.source(input.id))?;
        ensure!(
            after.cursor == before.cursor + result.consumed_bytes,
            "cursor not matching committed prefix"
        );
        progress.raw_pending = after.cursor < std::fs::metadata(&input.path)?.len();
        let view = self.active().with_active(|s| s.snapshot())?;
        if result.blocked {
            progress.refusals.push(json!({"cycle":progress.cycle,"source_id":input.id,"consumed_bytes":result.consumed_bytes,"facts_added":result.facts_added,"snapshot":view,"disk":disk(&self.samples.db)}));
            self.refusal(input, progress, result.consumed_bytes)
        } else {
            measured_operation!(self.send_queued(view.queued), self, progress);
            Ok(None)
        }
    }
    fn refusal(
        &mut self,
        input: &Input,
        progress: &mut Progress,
        consumed: u64,
    ) -> Result<Option<&'static str>> {
        if progress.recovery.is_none() {
            let fp = fingerprint(self.active())?;
            measured_operation!(
                operation("near/refusal reopen", self.reopen()),
                self,
                progress
            );
            ensure!(
                fingerprint(self.active())? == fp,
                "logical-refusal reopen changed projection"
            );
            let replay = measured_operation!(
                operation(
                    "logical-refusal replay/ACK",
                    self.delivery.recover_pending(self.owner.as_ref().unwrap())
                ),
                self,
                progress
            );
            progress.recovery = Some(
                json!({"projection_sha256":fp,"replay":replay,"disk_after":disk(&self.samples.db)}),
            );
        }
        while self.active().with_active(|s| Ok(s.snapshot()?.queued))? > 0 {
            measured_operation!(self.arm(), self, progress);
            measured_operation!(self.ack(), self, progress);
        }
        if consumed == 0 {
            let fp = fingerprint(self.active())?;
            let retry = measured_operation!(
                operation(
                    "stable-refusal-retry",
                    self.collect(input, "stable-refusal-retry")
                ),
                self,
                progress
            );
            if retry.blocked && retry.consumed_bytes == 0 {
                ensure!(
                    fingerprint(self.active())? == fp,
                    "stable refusal modified retained history"
                );
                return Ok(Some("logical_refusal_reopened_replayed_and_drained"));
            }
        }
        Ok(None)
    }
    fn control_probe(&mut self, input: &Input) -> Result<Value> {
        let ordinary_final = self.active().with_active(|s| s.snapshot())?;
        let history = self
            .active()
            .with_active(|s| s.audit_retained_fingerprint(&[]))?;
        let gap = self.active().with_active(|s| {
            s.collect_file(
                input.id,
                &input.file,
                self.delivery.time + 1,
                OverflowPolicy::GapAndHold,
            )
        })?;
        ensure!(
            gap.blocked && gap.facts_added == 1,
            "reserved spool-loss gap not admitted after ordinary refusal"
        );
        ensure!(
            self.active().with_active(|s| Ok(s.snapshot()?.seen))? == ordinary_final.seen,
            "control deleted/added native history"
        );
        self.advance_boot(66, 404)?;
        let packet = self
            .active()
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
            self.active()
                .with_active(|s| s.audit_retained_fingerprint(&control_ids))?
                == history,
            "control changed retained dedupe/event history"
        );
        let control_snapshot = self.active().with_active(|s| s.snapshot())?;
        let receipt = self.delivery.peer.receive(&packet)?;
        self.active().with_active(|s| s.acknowledge(&receipt))?;
        let history_after_control_ack = self
            .active()
            .with_active(|s| s.audit_retained_fingerprint(&[]))?;
        let sequence_after_control = self.verify_repeated_boots(&history_after_control_ack)?;
        Ok(
            json!({"gap_facts":gap.facts_added,"gap_consumed_bytes":gap.consumed_bytes,"boot_gap_facts":batch.events.len()-1,"total_urgent_facts":batch.events.len(),"pre_control":ordinary_final,"post_control_before_ack":control_snapshot,"retained_history_sha256":history,"history_preserved":true,"total_boot_transitions":64,"sequence_after_control":sequence_after_control,"repeated_boots_added_events":false}),
        )
    }
    fn advance_boot(&mut self, boot: u128, operation: u128) -> Result<()> {
        self.delivery.peer.node_boot = boot;
        drop(self.owner.take());
        let boot = Uuid::from_u128(boot);
        let mut next = LocalIntent::open(&self.root, boot)?;
        next.advance_boot(
            &mut self.delivery.peer,
            Uuid::from_u128(operation),
            boot,
            self.delivery.time + 1,
        )?;
        self.owner = Some(next);
        Ok(())
    }
    fn verify_repeated_boots(&mut self, history_after_control_ack: &str) -> Result<u64> {
        let sequence_after_control = self.active().with_active(|s| Ok(s.snapshot()?.next))?;
        // Exercise the full bounded boot metadata lifetime with no live sources.
        // Repeated boots cannot allocate another gap for an already-held source.
        for index in 1..64 {
            self.advance_boot(66 + index, 404 + index)?;
        }
        ensure!(
            self.active().with_active(|s| Ok(s.snapshot()?.next))? == sequence_after_control,
            "repeated held-source boot fabricated events"
        );
        ensure!(
            self.active()
                .with_active(|s| s.audit_retained_fingerprint(&[]))?
                == history_after_control_ack,
            "repeated boot changed retained history"
        );
        Ok(sequence_after_control)
    }
}
fn make_inputs(temp: &Path, owner: &LocalIntent, maximal_keys: bool) -> Result<Vec<Input>> {
    let mut inputs = vec![];
    for i in 0..16 {
        let mut s = source(i);
        if maximal_keys {
            s.native_session_key = Text::new("\"".repeat(512))?;
        }
        let raw = temp.join(format!("raw-{i}"));
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
        owner.with_active(|store| store.attach_file_engine(s.clone(), 4096, permit))?;
        inputs.push(Input {
            id: s.source_id,
            file,
            path,
            key: s.native_session_key.as_str().into(),
        });
    }
    Ok(inputs)
}
fn run(fat: bool) -> Result<Value> {
    run_configured(fat, Limits::fixture(), false)
}
fn run_configured(fat: bool, limits: Limits, maximal_keys: bool) -> Result<Value> {
    let temp = tempfile::tempdir()?;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
    let root = temp.path().join("owner");
    let mut experiment = Experiment::open(root, limits.clone())?;
    let inputs = make_inputs(temp.path(), experiment.active(), maximal_keys)?;
    let initial = experiment.active().with_active(|s| s.snapshot())?;
    let mut progress = Progress::default();
    let mut status = "bounded_work_limit_without_refusal";
    while progress.cycle < if fat { 8192 } else { 2048 } {
        if let Some(outcome) = experiment.step(&inputs, &mut progress, fat)? {
            status = outcome;
            break;
        }
    }
    let control_probe = experiment.control_probe(&inputs[progress.current])?;
    let final_view = if let Some(o) = &experiment.owner {
        Some(o.with_active(|s| s.snapshot())?)
    } else {
        None
    };
    let final_fingerprint = experiment.owner.as_ref().map(fingerprint);
    let final_fingerprint_error = final_fingerprint
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("{e:#}"));
    let final_fingerprint = final_fingerprint.and_then(Result::ok);
    let sources = experiment
        .owner
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
        json!({"fixture":"synthetic Claude-shaped data; managed fake transport authority, real file/normalizer/redb/CON03 pipeline; no native/version/production/source-authority qualification","mode":if fat{"32_fingerprints_per_call"}else{"32_unique_calls_per_batch"},"limits":limits,"status":status,"initial":initial,"near":progress.near,"first_refusals":progress.refusals,"recovery":progress.recovery,"control_probe":control_probe,"error_recovery":progress.error_recovery,"final":final_view,"final_projection_sha256":final_fingerprint,"final_fingerprint_error":final_fingerprint_error,"final_disk":disk(&experiment.samples.db),"sources":sources,"appended_batches":progress.cycle,"fake_central_through":experiment.delivery.peer.through,"unique_packet_receipts":experiment.delivery.peer.receipts.len(),"raw_fixture_bytes":inputs.iter().map(|i|std::fs::metadata(&i.path).unwrap().len()).sum::<u64>(),"samples":experiment.samples.rows,"compaction_called":false,"history_deleted":false}),
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
        let store = aq_observation_store::Store::open(&root.join("store"), Limits::fixture());
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
    use aq_observation_store::backend::{Action, Fault};
    #[test]
    fn prepare_send_and_ack_errors_keep_report_and_exact_recovery() {
        for stage in ["prepare", "send", "ack"] {
            let t = tempfile::tempdir().unwrap();
            let root = t.path().join("owner");
            let mut experiment = Experiment::open(root, Limits::fixture()).unwrap();
            experiment
                .active()
                .with_active(|s| {
                    s.attach(support::source(8))?;
                    support::append(s, 1);
                    Ok(())
                })
                .unwrap();
            if stage == "send" {
                experiment.active().with_active(|s| s.prepare()).unwrap();
            }
            if stage == "ack" {
                experiment
                    .delivery
                    .arm(experiment.owner.as_ref().unwrap(), &mut experiment.samples)
                    .unwrap();
            }
            let fp = fingerprint(experiment.active()).unwrap();
            let error = inject_failure(&experiment, stage);
            let report = experiment.failure_recovery(&error, stage, Some(&fp));
            assert_eq!(report["unchanged_projection"], true);
            assert!(report["pending_recovery_error"].is_null());
            let projection = experiment.active().with_active(|s| s.snapshot()).unwrap();
            assert_eq!(projection.seen, 1);
            assert_eq!(projection.next, 2);
            if stage != "prepare" {
                assert_eq!(projection.acked, 1);
            } else {
                experiment
                    .delivery
                    .arm(experiment.owner.as_ref().unwrap(), &mut experiment.samples)
                    .unwrap();
                experiment
                    .delivery
                    .ack(experiment.owner.as_ref().unwrap(), &mut experiment.samples)
                    .unwrap();
            }
        }
    }
    fn inject_failure(experiment: &Experiment, stage: &str) -> anyhow::Error {
        let owner = experiment.active();
        let monitor = owner.store_monitor().unwrap();
        monitor.reset();
        monitor.fault(Fault {
            operation: 1,
            after: false,
            action: Action::Error,
        });
        // Sample collection resets instrumentation intentionally, so inject directly around
        // the operation here, then exercise the same common recovery reporter.
        match stage {
            "prepare" => owner.with_active(|s| s.prepare().map(|_| ())).unwrap_err(),
            "send" => owner
                .with_active(|s| s.begin_send(1000).map(|_| ()))
                .unwrap_err(),
            _ => owner
                .with_active(|s| {
                    s.acknowledge(&experiment.delivery.pending.as_ref().unwrap().receipt)
                })
                .unwrap_err(),
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
