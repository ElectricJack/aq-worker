//! H5: physical suspension and continuation bootstrap over fake ports, with
//! `ConformancePeer` as the AQ lifecycle endpoint and CON-03 fixtures.
use anyhow::{Result, anyhow, bail};
use aq_lifecycle_protocol::*;
use aq_worker::hibernation::archive::SourceArchive;
use aq_worker::hibernation::continuation::{BOOTSTRAP_FILE, CONTINUATION_BOUNDARIES};
use aq_worker::hibernation::suspend::SUSPENSION_BOUNDARIES;
use aq_worker::hibernation::*;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

// ---------- fixtures ----------

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../aq-lifecycle-protocol/artifacts/fixtures.json"
    ))
    .unwrap()
}
fn packet(index: usize) -> Packet {
    serde_json::from_value(fixture()["valid_packets"][index].clone()).unwrap()
}
fn caps() -> Vec<Capability> {
    serde_json::from_value(fixture()["advertised_capabilities"].clone()).unwrap()
}
fn predecessor() -> Binding {
    serde_json::from_value(fixture()["predecessor"].clone()).unwrap()
}
fn successor() -> Binding {
    serde_json::from_value(fixture()["successor"].clone()).unwrap()
}
fn suspend_directive() -> Packet {
    packet(2)
}
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn summary_claim(text: &str) -> CheckpointClaim {
    let summary = SummaryClaim {
        text: SummaryText::new(text).unwrap(),
        context_references: vec![],
        pending_operation_ids: vec![Uuid::from_u128(0x32)],
    };
    CheckpointClaim::Supplied {
        summary_digest: canonical_digest(&summary).unwrap(),
        summary,
    }
}

const SUMMARY: &str =
    "Goal: finish H5. Done: journal. Next: wire continuation. Waiting on wait 14.";

// ---------- fakes ----------

struct FakeAq {
    peer: ConformancePeer,
    sent: Vec<Uuid>,
    drop_receipts: usize,
}
impl FakeAq {
    fn new(binding: Binding) -> Self {
        Self {
            peer: ConformancePeer::new(binding, caps()).unwrap(),
            sent: Vec::new(),
            drop_receipts: 0,
        }
    }
}
impl LifecycleApi for FakeAq {
    fn send(&mut self, packet: &Packet) -> Result<Receipt> {
        self.sent.push(packet.body.message_id);
        let receipt = self
            .peer
            .receive(packet)
            .map_err(|e| anyhow!("aq refused: {e}"))?;
        if self.drop_receipts > 0 {
            self.drop_receipts -= 1;
            bail!("receipt lost in transit");
        }
        Ok(receipt)
    }
}

/// Central retention shared by every node; no node-local cache.
#[derive(Default)]
struct FakeRetention {
    uploads: BTreeMap<Uuid, Vec<u8>>,
    committed: BTreeMap<Uuid, (RetentionReceipt, Vec<u8>)>,
    chunks: usize,
    commits: usize,
    corrupt: Option<ArtifactRole>,
    lose: Option<ArtifactRole>,
}
impl CentralRetention for FakeRetention {
    fn upload_chunk(&mut self, operation_id: Uuid, offset: u64, chunk: &[u8]) -> Result<u64> {
        self.chunks += 1;
        let buf = self.uploads.entry(operation_id).or_default();
        let offset = offset as usize;
        if offset > buf.len() {
            bail!("upload gap");
        }
        if offset < buf.len() {
            // Resumed chunk: must replay identical bytes.
            let end = (offset + chunk.len()).min(buf.len());
            if buf[offset..end] != chunk[..end - offset] {
                bail!("resumed chunk differs");
            }
            buf.truncate(offset);
        }
        buf.extend_from_slice(chunk);
        Ok(buf.len() as u64)
    }
    fn commit(
        &mut self,
        operation_id: Uuid,
        artifact: &ArtifactRef,
        role: ArtifactRole,
        binding: &Binding,
    ) -> Result<RetentionReceipt> {
        if let Some((receipt, _)) = self.committed.get(&operation_id) {
            return Ok(receipt.clone());
        }
        let bytes = self.uploads.get(&operation_id).cloned().unwrap_or_default();
        if Digest::of(&bytes) != artifact.digest {
            bail!("upload digest mismatch");
        }
        self.commits += 1;
        let receipt = RetentionReceipt {
            receipt_id: Uuid::new_v4(),
            owner: Text::new("central-retention-owner").unwrap(),
            binding_digest: canonical_digest(binding).unwrap(),
            artifact: artifact.clone(),
            role,
            bytes: bytes.len() as u64,
            retained_at_ms: 1500,
            retained_until_ms: 1_000_000,
        };
        self.committed
            .insert(operation_id, (receipt.clone(), bytes));
        Ok(receipt)
    }
    fn fetch(&self, receipt: &RetentionReceipt) -> Result<Vec<u8>> {
        if self.lose.as_ref() == Some(&receipt.role) {
            bail!("artifact missing from central retention");
        }
        let (_, bytes) = self
            .committed
            .values()
            .find(|(r, _)| r.receipt_id == receipt.receipt_id)
            .ok_or_else(|| anyhow!("unknown receipt"))?;
        let mut bytes = bytes.clone();
        if self.corrupt.as_ref() == Some(&receipt.role) {
            bytes[0] ^= 0xff;
        }
        Ok(bytes)
    }
}
impl FakeRetention {
    fn all_bytes(&self) -> Vec<(ArtifactRole, Vec<u8>)> {
        self.committed
            .values()
            .map(|(r, b)| (r.role.clone(), b.clone()))
            .collect()
    }
}

struct FakeNode {
    claim: CheckpointClaim,
    stop: StopObservation,
    closes: usize,
    stops: usize,
    reconciles: usize,
    computes: usize,
    disks: usize,
    log: RefCell<Vec<&'static str>>,
}
impl FakeNode {
    fn new() -> Self {
        Self {
            claim: summary_claim(SUMMARY),
            stop: StopObservation::ReportedStopped {
                observation_id: Uuid::from_u128(0x40),
                guardian_instance_id: Uuid::from_u128(0x41),
                writer_set_digest: Digest::of(b"writers"),
                writer_count: 2,
                observed_at_ms: 1200,
                proof: artifact("guardian-stop-proof"),
            },
            closes: 0,
            stops: 0,
            reconciles: 0,
            computes: 0,
            disks: 0,
            log: RefCell::new(Vec::new()),
        }
    }
}
impl SuspensionPorts for FakeNode {
    fn request_checkpoint(&mut self, intent: &SuspensionIntent) -> Result<CheckpointClaim> {
        assert_eq!(intent.max_summary_bytes, 16384);
        self.log.borrow_mut().push("checkpoint");
        Ok(self.claim.clone())
    }
    fn close_input(&mut self) -> Result<()> {
        self.closes += 1;
        self.log.borrow_mut().push("close_input");
        Ok(())
    }
    fn stop_writers(&mut self) -> Result<StopObservation> {
        self.stops += 1;
        self.log.borrow_mut().push("stop");
        Ok(self.stop.clone())
    }
    fn reconcile_effects(&mut self, pending: &[Uuid]) -> Result<EffectsObservation> {
        assert_eq!(pending, [Uuid::from_u128(0x32)]);
        self.reconciles += 1;
        Ok(EffectsObservation::Reconciled {
            receipt: artifact("effects-reconciliation"),
        })
    }
    fn release_compute(&mut self, stop: &StopObservation) -> Result<()> {
        assert!(matches!(stop, StopObservation::ReportedStopped { .. }));
        self.computes += 1;
        self.log.borrow_mut().push("release_compute");
        Ok(())
    }
    fn release_disk(&mut self, _directive: &ArtifactRef) -> Result<()> {
        self.disks += 1;
        Ok(())
    }
}

/// Fresh tmux host stand-in: issues new credentials into the new sandbox.
#[derive(Default)]
struct FakeLauncher {
    launches: Vec<(Binding, PathBuf)>,
}
impl SessionLauncher for FakeLauncher {
    fn launch(
        &mut self,
        successor: &Binding,
        layout: &CredentialLayout,
        bootstrap: &Path,
    ) -> Result<()> {
        assert!(bootstrap.exists(), "bootstrap instructions precede launch");
        for path in layout.paths() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(
                &path,
                format!("fresh-{}-{}", successor.session_id(), Uuid::new_v4()),
            )?;
        }
        self.launches
            .push((successor.clone(), bootstrap.to_owned()));
        Ok(())
    }
}

// ---------- sandbox helpers ----------

struct Node {
    _tmp: tempfile::TempDir,
    journal: PathBuf,
    layout: CredentialLayout,
}
fn node() -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let layout = CredentialLayout::new(tmp.path().join("runner"), tmp.path().join("work"));
    std::fs::create_dir_all(&layout.runner_root).unwrap();
    std::fs::create_dir_all(&layout.work_root).unwrap();
    Node {
        journal: tmp.path().join("journal"),
        layout,
        _tmp: tmp,
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=h5", "-c", "user.email=h5@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A workspace with a commit, a dirty tracked file and an untracked file.
fn dirty_workspace(layout: &CredentialLayout) {
    let repo = layout.workspace();
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "committed\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    std::fs::write(repo.join("tracked.txt"), "committed\ndirty edit\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "new work\n").unwrap();
}

/// Plant every credential kind; returns the planted secret strings.
fn plant_credentials(layout: &CredentialLayout) -> Vec<String> {
    layout
        .paths()
        .iter()
        .enumerate()
        .map(|(i, path)| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let secret = format!("planted-secret-{i}-{}", Uuid::new_v4());
            std::fs::write(path, &secret).unwrap();
            secret
        })
        .collect()
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(all_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn hibernate(a: &Node) -> (Suspended, FakeRetention, FakeAq, FakeNode) {
    let mut ports = FakeNode::new();
    let mut aq = FakeAq::new(predecessor());
    let mut retention = FakeRetention::default();
    let parked = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .with_chunk_bytes(4096)
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    (parked, retention, aq, ports)
}

/// AQ issues the continuation to the successor grant from the parked receipts.
fn continuation_packet(parked: &Suspended) -> Packet {
    let template = packet(4);
    let Message::Continuation(mut attachment) = template.body.message.clone() else {
        unreachable!()
    };
    attachment.retained = parked.report.central_receipts.clone();
    Packet::seal(template.body.message_id, Message::Continuation(attachment)).unwrap()
}

fn attachment_of(p: &Packet) -> ContinuationAttachment {
    match &p.body.message {
        Message::Continuation(a) => a.as_ref().clone(),
        _ => unreachable!(),
    }
}

fn ack_for(woken: &Woken, attachment: &ContinuationAttachment) -> BootstrapAck {
    BootstrapAck {
        admission_id: attachment.admission_id,
        continuation: attachment.continuation.clone(),
        successor: attachment.successor.clone(),
        attachment_digest: woken.attachment_digest.clone(),
        outcome: attachment.outcome.clone(),
        acknowledgment_receipt: artifact("authenticated-aq-bootstrap-ack"),
    }
}

// ---------- suspension ----------

#[test]
fn hibernation_leaves_no_credentials_in_retained_state() {
    let a = node();
    dirty_workspace(&a.layout);
    let secrets = plant_credentials(&a.layout);
    let (parked, retention, _aq, ports) = hibernate(&a);

    assert_eq!(
        parked.report.disposition,
        SuspensionDisposition::RetainedSuspended
    );
    assert!(parked.report.compute_released && !parked.report.protected_disk_released);
    assert_eq!(
        parked.receipt.disposition,
        ReceiptDisposition::ParkingRecorded
    );
    assert_eq!(ports.stops, 1);
    for path in a.layout.paths() {
        assert!(
            !path.exists(),
            "credential {} survived hibernation",
            path.display()
        );
    }

    let retained = retention.all_bytes();
    assert_eq!(
        retained.len(),
        2,
        "source and checkpoint retained centrally"
    );
    for (role, bytes) in &retained {
        for secret in &secrets {
            assert!(
                !contains(bytes, secret),
                "{role:?} artifact holds a credential"
            );
        }
        if *role == ArtifactRole::Source {
            let archive = SourceArchive::from_bytes(bytes).unwrap();
            for (path, data) in archive.decoded().unwrap() {
                for known in [
                    "permit.json",
                    "provider.key",
                    "access.json",
                    ".credentials.json",
                ] {
                    assert!(!path.ends_with(known), "retained credential path {path}");
                }
                assert!(!path.contains("agent-home"), "retained login path {path}");
                for secret in &secrets {
                    assert!(!contains(&data, secret));
                }
            }
        }
    }
    // The local journal and protected snapshot never hold secret bytes either.
    for file in all_files(&a.journal) {
        let bytes = std::fs::read(&file).unwrap();
        for secret in &secrets {
            assert!(
                !contains(&bytes, secret),
                "{} holds a credential",
                file.display()
            );
        }
    }

    // A credential copied into the workspace is an explicit hold: no retention.
    let b = node();
    dirty_workspace(&b.layout);
    let secrets = plant_credentials(&b.layout);
    std::fs::write(
        b.layout.workspace().join("notes.txt"),
        format!("token: {}\n", secrets[0]),
    )
    .unwrap();
    let (held, retention, _aq, ports) = hibernate(&b);
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert!(retention.committed.is_empty() && retention.chunks == 0);
    assert!(!held.report.compute_released && ports.computes == 0);
    for path in b.layout.paths() {
        assert!(
            !path.exists(),
            "credentials are scrubbed after stop even when held"
        );
    }

    // A credential leaked into the agent summary also holds.
    let c = node();
    dirty_workspace(&c.layout);
    let secrets = plant_credentials(&c.layout);
    let mut ports = FakeNode::new();
    ports.claim = summary_claim(&format!("my key is {}", secrets[1]));
    let mut aq = FakeAq::new(predecessor());
    let mut retention = FakeRetention::default();
    let held = Suspender::new(&c.journal, predecessor(), c.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert!(retention.committed.is_empty());
}

#[test]
fn suspension_recovers_at_every_boundary() {
    for boundary in SUSPENSION_BOUNDARIES {
        let a = node();
        dirty_workspace(&a.layout);
        plant_credentials(&a.layout);
        let mut ports = FakeNode::new();
        let mut aq = FakeAq::new(predecessor());
        let mut retention = FakeRetention::default();
        let err = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .with_chunk_bytes(4096)
            .with_boundaries(Boundaries::crash_after(boundary))
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap_err();
        assert!(
            err.downcast_ref::<Crashed>().is_some(),
            "{boundary}: {err:#}"
        );

        let mut recovered =
            Suspender::new(&a.journal, predecessor(), a.layout.clone()).with_chunk_bytes(4096);
        let parked = recovered
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap_or_else(|e| panic!("{boundary}: recovery failed: {e:#}"));
        assert_eq!(
            parked.report.disposition,
            SuspensionDisposition::RetainedSuspended,
            "{boundary}"
        );
        parked.report.validate().unwrap();
        parked.receipt.validate().unwrap();
        assert_eq!(ports.stops, 1, "{boundary}: writers stopped once");
        assert_eq!(ports.closes, 1, "{boundary}: input closed once");
        assert_eq!(ports.computes, 1, "{boundary}: compute released once");
        assert_eq!(retention.commits, 2, "{boundary}: one receipt per artifact");
        assert_eq!(parked.report.central_receipts.len(), 2);
        assert_eq!(
            ports
                .log
                .borrow()
                .iter()
                .filter(|s| **s == "checkpoint")
                .count(),
            1,
            "{boundary}: checkpoint requested once"
        );
        // Effects are ordered: checkpoint, close, stop, then compute release.
        assert_eq!(
            *ports.log.borrow(),
            ["checkpoint", "close_input", "stop", "release_compute"],
            "{boundary}"
        );
        // A repeated directive returns the same transition, not a new one.
        let again = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap();
        assert_eq!(again, parked, "{boundary}: repeat recovers the same report");
        let distinct: BTreeSet<_> = aq.sent.iter().collect();
        assert_eq!(
            distinct.len(),
            2,
            "{boundary}: one checkpoint + one parking message id"
        );
    }
}

#[test]
fn suspension_refuses_conflicting_or_stale_directive() {
    let a = node();
    dirty_workspace(&a.layout);
    let (_parked, mut retention, mut aq, mut ports) = hibernate(&a);
    let mut other = match suspend_directive().body.message {
        Message::Suspend(i) => i,
        _ => unreachable!(),
    };
    other.deadline_ms += 1;
    let conflicting = Packet::seal(Uuid::new_v4(), Message::Suspend(other.clone())).unwrap();
    assert!(
        Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .suspend(&conflicting, &mut ports, &mut aq, &mut retention)
            .is_err()
    );
    other.binding = successor();
    let stale = Packet::seal(Uuid::new_v4(), Message::Suspend(other)).unwrap();
    let b = node();
    assert!(
        Suspender::new(&b.journal, predecessor(), b.layout.clone())
            .suspend(&stale, &mut ports, &mut aq, &mut retention)
            .is_err()
    );
}

#[test]
fn unknown_stop_holds_without_snapshot_or_release() {
    let a = node();
    dirty_workspace(&a.layout);
    plant_credentials(&a.layout);
    let mut ports = FakeNode::new();
    ports.stop = StopObservation::Unknown {
        reason: Text::new("guardian lost writer ownership").unwrap(),
    };
    let mut aq = FakeAq::new(predecessor());
    let mut retention = FakeRetention::default();
    let held = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert_eq!(
        held.receipt.disposition,
        ReceiptDisposition::ParkingRecorded
    );
    assert!(!held.report.compute_released && ports.computes == 0);
    assert!(retention.chunks == 0 && retention.committed.is_empty());
    assert!(held.report.central_receipts.is_empty());
    assert!(
        Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .release_disk(&artifact("disk-release"), &mut ports, &mut aq)
            .is_err(),
        "held suspension cannot release protected disk"
    );
    assert_eq!(ports.disks, 0);
}

#[test]
fn protected_disk_released_only_by_explicit_directive() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, _retention, mut aq, mut ports) = hibernate(&a);
    assert!(parked.report.compute_released);
    assert!(
        !parked.report.protected_disk_released && parked.report.disk_release_directive.is_none()
    );
    assert!(
        parked.report.local_retained_bytes > 0,
        "snapshot stays on protected disk"
    );
    assert_eq!(ports.disks, 0, "compute release does not release disk");
    let directive = artifact("disk-release-directive");
    let released = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .release_disk(&directive, &mut ports, &mut aq)
        .unwrap();
    assert!(released.report.protected_disk_released);
    assert_eq!(
        released.report.disk_release_directive,
        Some(directive.clone())
    );
    assert_eq!(released.report.local_retained_bytes, 0);
    assert_eq!(ports.disks, 1);
    // Idempotent under the same directive.
    Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .release_disk(&directive, &mut ports, &mut aq)
        .unwrap();
    assert_eq!(ports.disks, 1);
}

// ---------- continuation ----------

#[test]
fn continuation_hydrates_checkpoint_in_new_sandbox() {
    // Node A hibernates, then disappears entirely (no local cache survives).
    let a = node();
    dirty_workspace(&a.layout);
    let old_secrets = plant_credentials(&a.layout);
    let expected_status = git(&a.layout.workspace(), &["status", "--porcelain"]);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    drop(a);

    // AQ grants a fresh attempt on node B with the continuation attachment.
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();

    // Source, including dirty and untracked work, is restored from retention.
    let repo = b.layout.workspace();
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "committed\ndirty edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("untracked.txt")).unwrap(),
        "new work\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), expected_status);
    assert!(
        expected_status.contains(" M tracked.txt") && expected_status.contains("?? untracked.txt")
    );

    // The summary and exact generation are in the bootstrap instructions.
    assert_eq!(woken.summary, summary_claim(SUMMARY));
    let bootstrap: Value =
        serde_json::from_slice(&std::fs::read(b.layout.work_root.join(BOOTSTRAP_FILE)).unwrap())
            .unwrap();
    assert_eq!(bootstrap["continuation"]["generation"], 1);
    assert_eq!(bootstrap["summary"]["summary"]["text"], SUMMARY);
    assert_eq!(
        bootstrap["attachment_digest"],
        woken.attachment_digest.as_str()
    );

    // A fresh session started for the successor with fresh credentials only.
    assert_eq!(launcher.launches.len(), 1);
    assert_eq!(launcher.launches[0].0, successor());
    for path in b.layout.paths() {
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert!(fresh.starts_with(&format!("fresh-{}", successor().session_id())));
        assert!(!old_secrets.contains(&fresh));
    }

    // Operations stay closed until AQ records the exact BootstrapAck.
    assert_eq!(woken.gate, OperationsGate::AwaitingBootstrapAck);
    assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    assert!(!aq_b.peer.bootstrap_acknowledged());
    let gate = boot
        .acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
        .unwrap();
    assert_eq!(gate, OperationsGate::Open);
    assert!(aq_b.peer.bootstrap_acknowledged());
    assert_eq!(aq_b.sent.last(), Some(&woken.ack_message_id));

    // A replayed attachment recovers the same wake without relaunching.
    let again = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .wake(&wake, &retention, &mut launcher)
        .unwrap();
    assert_eq!(again.gate, OperationsGate::Open);
    assert_eq!(launcher.launches.len(), 1);
}

#[test]
fn lost_bootstrap_ack_retries_with_same_receipt_identity() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let woken = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .wake(&wake, &retention, &mut launcher)
        .unwrap();
    let ack = ack_for(&woken, &attachment);

    aq_b.drop_receipts = 1;
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    assert!(boot.acknowledge(&ack, &mut aq_b).is_err());
    assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    // Crash/restart: the retry reuses the journaled message id; AQ dedups it.
    let gate = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .acknowledge(&ack, &mut aq_b)
        .unwrap();
    assert_eq!(gate, OperationsGate::Open);
    assert_eq!(aq_b.sent, vec![woken.ack_message_id, woken.ack_message_id]);
}

#[test]
fn stale_bootstrap_ack_cannot_open_operations() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();

    let mut from_predecessor = ack_for(&woken, &attachment);
    from_predecessor.successor = predecessor();
    let mut old_generation = ack_for(&woken, &attachment);
    old_generation.continuation.generation = 2;
    let mut other_attachment = ack_for(&woken, &attachment);
    other_attachment.attachment_digest = Digest::of(b"other attachment");
    for stale in [from_predecessor, old_generation, other_attachment] {
        assert!(boot.acknowledge(&stale, &mut aq_b).is_err());
        assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    }
    assert!(
        aq_b.sent.is_empty(),
        "stale acks never reach AQ as this attempt"
    );
    assert!(!aq_b.peer.bootstrap_acknowledged());
    // The exact ack still opens operations afterwards.
    assert_eq!(
        boot.acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
            .unwrap(),
        OperationsGate::Open
    );
}

#[test]
fn missing_or_corrupt_retained_artifact_refuses_wake() {
    for (lose, corrupt) in [
        (Some(ArtifactRole::Checkpoint), None),
        (Some(ArtifactRole::Source), None),
        (None, Some(ArtifactRole::Source)),
        (None, Some(ArtifactRole::Checkpoint)),
    ] {
        let a = node();
        dirty_workspace(&a.layout);
        let (parked, mut retention, _aq, _ports) = hibernate(&a);
        retention.lose = lose.clone();
        retention.corrupt = corrupt.clone();
        let wake = continuation_packet(&parked);
        let b = node();
        let mut launcher = FakeLauncher::default();
        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
        assert!(
            boot.wake(&wake, &retention, &mut launcher).is_err(),
            "{lose:?} {corrupt:?}"
        );
        assert!(
            launcher.launches.is_empty(),
            "no session without verified inputs"
        );
        assert!(!b.layout.workspace().exists(), "nothing hydrated");
        assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    }
    // A wake attachment for another grant is refused outright.
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let b = node();
    let mut launcher = FakeLauncher::default();
    let other_grant = predecessor();
    assert!(
        ContinuationBootstrap::new(&b.journal, other_grant, b.layout.clone())
            .wake(&continuation_packet(&parked), &retention, &mut launcher)
            .is_err()
    );
}

#[test]
fn continuation_recovers_at_every_boundary() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    for boundary in CONTINUATION_BOUNDARIES {
        let mut aq_b = FakeAq::new(successor());
        aq_b.peer.receive(&wake).unwrap();
        let b = node();
        let mut launcher = FakeLauncher::default();
        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
            .with_boundaries(Boundaries::crash_after(boundary));
        let crashed = boot.wake(&wake, &retention, &mut launcher).and_then(|w| {
            boot.acknowledge(&ack_for(&w, &attachment), &mut aq_b)
                .map(|_| w)
        });
        let err = crashed.unwrap_err();
        assert!(
            err.downcast_ref::<Crashed>().is_some(),
            "{boundary}: {err:#}"
        );
        // Only a journaled BootstrapAcknowledgmentRecorded receipt opens the gate.
        let expected = if *boundary == "ack_recorded" {
            OperationsGate::Open
        } else {
            OperationsGate::AwaitingBootstrapAck
        };
        assert_eq!(boot.gate().unwrap(), expected, "{boundary}");

        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
        let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();
        let gate = boot
            .acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
            .unwrap();
        assert_eq!(gate, OperationsGate::Open, "{boundary}");
        assert_eq!(
            launcher.launches.len(),
            1,
            "{boundary}: exactly one fresh session"
        );
        assert!(aq_b.peer.bootstrap_acknowledged(), "{boundary}");
        assert!(
            aq_b.sent.iter().all(|id| *id == woken.ack_message_id),
            "{boundary}"
        );
        assert_eq!(
            std::fs::read_to_string(b.layout.workspace().join("untracked.txt")).unwrap(),
            "new work\n",
            "{boundary}"
        );
    }
}
