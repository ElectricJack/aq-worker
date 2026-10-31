//! Pure authority fakes; these cases do not qualify a real tmux host.
use aq_session_host::{
    GuardianIdentity, LaunchIntent, PaneIdentity, ProcessEvidence, terminal_io::*,
};
use aq_session_protocol::*;
use aq_terminal_bridge::*;
use aq_terminal_relay_protocol::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

struct FakeHost {
    binding: TerminalBinding,
    state: Mutex<FakeState>,
}
#[derive(Default)]
struct FakeState {
    submissions: usize,
    physical: usize,
    refused: usize,
    receipts: BTreeMap<Uuid, Receipt>,
    fail_after_effect: bool,
    pending: bool,
    block_receipts: bool,
}
impl FakeHost {
    fn new() -> Self {
        let terminal = TerminalIdentity {
            session: SessionIdentity::Remote {
                node_id: id(1),
                boot_id: id(2),
                attempt_id: id(3),
                assignment_generation: 1,
                session_id: id(4),
            },
            terminal_id: id(5),
            epoch: 1,
        };
        Self {
            binding: TerminalBinding {
                directory: DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
                terminal: terminal.clone(),
                guardian: GuardianIdentity {
                    intent: LaunchIntent {
                        launch_id: id(6),
                        session: terminal.session,
                        manifest_digest: Digest::of(b"manifest"),
                        configuration_digest: Digest::of(b"config"),
                    },
                    nonce: id(7),
                    process: ProcessEvidence {
                        pid: 100,
                        start_ticks: 1,
                    },
                    machine_boot_id: id(9).to_string(),
                    pid_namespace: "pid:[123]".into(),
                },
                pane: PaneIdentity {
                    server: ProcessEvidence {
                        pid: 101,
                        start_ticks: 1,
                    },
                    pane: ProcessEvidence {
                        pid: 102,
                        start_ticks: 1,
                    },
                    pane_id: "%0".into(),
                    session_name: "aq".into(),
                },
            },
            state: Mutex::new(FakeState::default()),
        }
    }
    fn peer(&self) -> Peer {
        Peer::Remote {
            node_id: id(1),
            boot_id: id(2),
        }
    }
    fn input(&self, n: u128) -> InputCommand {
        InputCommand {
            operation_id: id(n),
            lease: ControllerLease {
                terminal: self.binding.terminal.clone(),
                principal_id: Text::new("synthetic").unwrap(),
                lease_id: id(8),
                generation: 1,
                issued_at_ms: 1000,
                expires_at_ms: 10000,
                execution_expires_at_ms: 20000,
            },
            issued_at_ms: 2000,
            deadline_ms: 3000,
            bytes: b"private-input\n".to_vec(),
        }
    }
}
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn digest(v: &impl serde::Serialize) -> Digest {
    let mut value = serde_json::to_value(v).unwrap();
    value.sort_all_objects();
    Digest::of(&serde_json::to_vec(&value).unwrap())
}
impl OwnedTerminal for FakeHost {
    fn binding(&self) -> &TerminalBinding {
        &self.binding
    }
    fn submit(&self, effect: &Effect, _: QualifiedClock) -> anyhow::Result<Option<Receipt>> {
        let mut state = self.state.lock().unwrap();
        state.submissions += 1;
        let (id, receipt) = match effect {
            Effect::Input(v) => {
                if let Some(old) = state.receipts.get(&v.operation_id) {
                    return Ok(Some(old.clone()));
                }
                if state.pending {
                    return Ok(None);
                }
                state.physical += 1;
                let disposition = if state.fail_after_effect {
                    InputDisposition::DeliveryUnknown
                } else {
                    InputDisposition::Applied
                };
                (
                    v.operation_id,
                    Receipt::Input(InputReceipt {
                        operation_id: v.operation_id,
                        command_digest: digest(v),
                        terminal: v.lease.terminal.clone(),
                        disposition,
                    }),
                )
            }
            Effect::Fence(v) => (
                v.fence_id,
                Receipt::Fence(FenceAck {
                    terminal: v.terminal.clone(),
                    fence_id: v.fence_id,
                    generation: v.generation,
                }),
            ),
            Effect::Resize(v) => (
                v.operation_id,
                Receipt::Resize(ResizeReceipt {
                    terminal: v.lease.terminal.clone(),
                    operation_id: v.operation_id,
                    command_digest: digest(v),
                    sequence: v.sequence,
                    applied: true,
                }),
            ),
            _ => unreachable!(),
        };
        state.receipts.insert(id, receipt.clone());
        if state.fail_after_effect {
            anyhow::bail!("synthetic lost result after physical effect");
        }
        Ok(Some(receipt))
    }
    fn receipt(&self, id: Uuid) -> anyhow::Result<Option<Receipt>> {
        let state = self.state.lock().unwrap();
        anyhow::ensure!(!state.block_receipts, "synthetic guardian busy");
        Ok(state.receipts.get(&id).cloned())
    }
    fn reject_unstarted_input(
        &self,
        operation_id: Uuid,
        command_digest: &Digest,
    ) -> anyhow::Result<Receipt> {
        let mut state = self.state.lock().unwrap();
        if let Some(old) = state.receipts.get(&operation_id) {
            return Ok(old.clone());
        }
        state.refused += 1;
        let receipt = Receipt::Input(InputReceipt {
            operation_id,
            command_digest: command_digest.clone(),
            terminal: self.binding.terminal.clone(),
            disposition: InputDisposition::Rejected,
        });
        state.receipts.insert(operation_id, receipt.clone());
        Ok(receipt)
    }
}
fn open(temp: &tempfile::TempDir, host: &Arc<FakeHost>) -> Bridge {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let clock = QualifiedClock::assume_bounded_skew(0).unwrap();
    let mut bridge = if temp.path().join("owner.lock").exists() {
        Bridge::open(temp.path(), host.peer(), clock)
    } else {
        Bridge::create(temp.path(), host.peer(), clock)
    }
    .unwrap();
    bridge.attach(host.clone()).unwrap();
    // Explicit synthetic coordinator confirmation, not real grant evidence.
    let registration = bridge
        .prepare_registration(&host.binding.terminal, 1)
        .unwrap();
    bridge.confirm_registration(&registration).unwrap();
    bridge
}
fn envelope(host: &FakeHost, cursor: u64, effects: Vec<Effect>) -> Envelope {
    Envelope {
        version: VERSION,
        capability: Text::new(CAPABILITY).unwrap(),
        peer: host.peer(),
        exchange_id: Uuid::new_v4(),
        cursor,
        effects,
    }
}
fn ack(upload: &PeerUpload) -> UploadAck {
    UploadAck {
        version: VERSION,
        capability: upload.capability.clone(),
        peer: upload.peer.clone(),
        exchange_id: upload.exchange_id,
        cursor: upload.cursor,
        retirements: vec![],
        dropped_output: vec![],
    }
}

#[test]
fn lost_network_ack_restarts_with_identical_metadata_never_another_input() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    let input = host.input(10);
    let command = envelope(&host, 1, vec![Effect::Input(input.clone())]);
    let prepared = bridge.receive(&command).unwrap().pop().unwrap();
    bridge.execute(prepared).unwrap();
    let upload = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(host.state.lock().unwrap().physical, 1);
    assert!(upload.frames.is_empty() && upload.snapshots.is_empty() && upload.gaps.is_empty());
    drop(bridge);
    let mut bridge = open(&temp, &host);
    let retry = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&upload).unwrap(),
        serde_json::to_vec(&retry).unwrap()
    );
    assert!(bridge.receive(&command).unwrap().is_empty());
    let mut wrong = ack(&retry);
    wrong.exchange_id = id(99);
    assert!(bridge.acknowledge_control_upload(&wrong).is_err());
    bridge.acknowledge_control_upload(&ack(&retry)).unwrap();
    assert!(bridge.prepare_control_upload().unwrap().is_none());
    assert_eq!(host.state.lock().unwrap().submissions, 1);
    let raw = std::fs::read(temp.path().join("bridge.json")).unwrap();
    assert!(!raw.windows(input.bytes.len()).any(|v| v == input.bytes));
}

#[test]
fn lost_preparation_is_durably_refused_and_stale_token_cannot_inject() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    let stale = bridge
        .receive(&envelope(&host, 1, vec![Effect::Input(host.input(10))]))
        .unwrap()
        .pop()
        .unwrap();
    drop(bridge);
    let mut bridge = open(&temp, &host);
    assert_eq!(bridge.recover_pending().unwrap(), 1);
    bridge.execute(stale).unwrap();
    let upload = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(upload.receipts[0].disposition, InputDisposition::Rejected);
    let state = host.state.lock().unwrap();
    assert_eq!(state.physical, 0);
    assert_eq!(state.refused, 1);
}

#[test]
fn lost_host_reply_after_effect_recovers_unknown_without_resubmission() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    host.state.lock().unwrap().fail_after_effect = true;
    let mut bridge = open(&temp, &host);
    let command = envelope(&host, 1, vec![Effect::Input(host.input(10))]);
    let prepared = bridge.receive(&command).unwrap().pop().unwrap();
    assert!(bridge.execute(prepared).is_err());
    drop(bridge);
    let mut bridge = open(&temp, &host);
    assert_eq!(bridge.recover_pending().unwrap(), 1);
    assert!(bridge.receive(&command).unwrap().is_empty());
    assert_eq!(
        bridge.prepare_control_upload().unwrap().unwrap().receipts[0].disposition,
        InputDisposition::DeliveryUnknown
    );
    let state = host.state.lock().unwrap();
    assert_eq!(state.submissions, 1);
    assert_eq!(state.physical, 1);
}

#[test]
fn changed_bytes_stale_boot_epoch_and_host_replacement_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    bridge
        .receive(&envelope(&host, 1, vec![Effect::Input(host.input(10))]))
        .unwrap();
    bridge
        .receive(&envelope(&host, 2, vec![Effect::Input(host.input(11))]))
        .unwrap();
    let mut changed = host.input(10);
    changed.bytes = b"changed\n".to_vec();
    assert!(
        bridge
            .receive(&envelope(&host, 3, vec![Effect::Input(changed)]))
            .is_err()
    );
    let mut stale = host.input(12);
    stale.lease.terminal.epoch += 1;
    assert!(
        bridge
            .receive(&envelope(&host, 3, vec![Effect::Input(stale)]))
            .is_err()
    );
    let mut wrong_boot = envelope(&host, 3, vec![Effect::Input(host.input(12))]);
    wrong_boot.peer = Peer::Remote {
        node_id: id(1),
        boot_id: id(99),
    };
    assert!(bridge.receive(&wrong_boot).is_err());
    let mut replacement = FakeHost::new();
    replacement.binding.guardian.nonce = id(100);
    assert!(bridge.attach(Arc::new(replacement)).is_err());
    assert_eq!(bridge.operation_count(), 2);
    assert_eq!(host.state.lock().unwrap().physical, 0);
}

#[test]
fn finite_history_refuses_new_ids_but_keeps_exact_retries_and_reserved_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    for batch in 0..MAX_OPERATIONS / MAX_BATCH {
        let effects = (0..MAX_BATCH)
            .map(|n| Effect::Input(host.input(1000 + (batch * MAX_BATCH + n) as u128)))
            .collect();
        assert_eq!(
            bridge
                .receive(&envelope(&host, batch as u64, effects))
                .unwrap()
                .len(),
            MAX_BATCH
        );
    }
    assert_eq!(bridge.operation_count(), MAX_OPERATIONS);
    assert!(
        bridge
            .receive(&envelope(&host, 100, vec![Effect::Input(host.input(9000))]))
            .is_err()
    );
    assert!(
        bridge
            .receive(&envelope(&host, 100, vec![Effect::Input(host.input(1000))]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(bridge.recover_pending().unwrap(), MAX_BATCH);
    let upload = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(upload.receipts.len(), MAX_BATCH);
    assert!(serde_json::to_vec(&upload).unwrap().len() < 256 * 1024);
}

#[test]
fn corruption_missing_journal_and_competing_owner_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    assert!(
        Bridge::open(
            temp.path(),
            host.peer(),
            QualifiedClock::assume_bounded_skew(0).unwrap()
        )
        .is_err()
    );
    bridge
        .receive(&envelope(&host, 1, vec![Effect::Input(host.input(10))]))
        .unwrap();
    drop(bridge);
    let path = temp.path().join("bridge.json");
    let original = std::fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value["state"]["cursor"] = serde_json::json!(999);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        Bridge::open(
            temp.path(),
            host.peer(),
            QualifiedClock::assume_bounded_skew(0).unwrap()
        )
        .is_err()
    );
    std::fs::write(&path, original).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(
        Bridge::open(
            temp.path(),
            host.peer(),
            QualifiedClock::assume_bounded_skew(0).unwrap()
        )
        .is_err()
    );
}

fn private_file(path: &std::path::Path, bytes: &[u8]) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn root_and_lock_replacement_poison_the_old_owner_and_cannot_adopt_copied_history() {
    use std::os::unix::fs::DirBuilderExt;
    for replace_root in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("spool");
        let host = Arc::new(FakeHost::new());
        let clock = QualifiedClock::assume_bounded_skew(0).unwrap();
        let mut bridge = Bridge::create(&root, host.peer(), clock).unwrap();
        bridge.attach(host.clone()).unwrap();
        let registration = bridge
            .prepare_registration(&host.binding.terminal, 1)
            .unwrap();
        bridge.confirm_registration(&registration).unwrap();
        bridge
            .receive(&envelope(&host, 1, vec![Effect::Input(host.input(10))]))
            .unwrap();
        if replace_root {
            std::fs::rename(&root, parent.path().join("retained")).unwrap();
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .unwrap();
            let original = std::fs::read(parent.path().join("retained/bridge.json")).unwrap();
            private_file(&root.join("bridge.json"), &original);
            private_file(&root.join("owner.lock"), b"");
        } else {
            std::fs::rename(root.join("owner.lock"), root.join("retained.lock")).unwrap();
            private_file(&root.join("owner.lock"), b"");
        }
        assert!(bridge.poll_request(0).is_err());
        assert!(Bridge::open(&root, host.peer(), clock).is_err());
        if replace_root {
            std::fs::remove_dir_all(&root).unwrap();
            std::fs::rename(parent.path().join("retained"), &root).unwrap();
        } else {
            std::fs::remove_file(root.join("owner.lock")).unwrap();
            std::fs::rename(root.join("retained.lock"), root.join("owner.lock")).unwrap();
        }
        assert!(
            bridge
                .receive(&envelope(&host, 2, vec![Effect::Input(host.input(11))]))
                .is_err()
        );
        assert_eq!(host.state.lock().unwrap().physical, 0);
        drop(bridge);
        let reopened = Bridge::open(&root, host.peer(), clock).unwrap();
        assert_eq!(reopened.operation_count(), 1);
    }
}

#[test]
fn identical_journal_inode_replacement_is_detected_while_owner_is_live() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    bridge
        .receive(&envelope(&host, 1, vec![Effect::Input(host.input(10))]))
        .unwrap();
    let bytes = std::fs::read(temp.path().join("bridge.json")).unwrap();
    std::fs::rename(
        temp.path().join("bridge.json"),
        temp.path().join("retained.json"),
    )
    .unwrap();
    private_file(&temp.path().join("bridge.json"), &bytes);
    assert!(bridge.prepare_control_upload().is_err());
    std::fs::remove_file(temp.path().join("bridge.json")).unwrap();
    std::fs::rename(
        temp.path().join("retained.json"),
        temp.path().join("bridge.json"),
    )
    .unwrap();
    assert!(bridge.poll_request(0).is_err()); // Restoration does not unpoison.
}

#[test]
fn two_exclusive_installers_have_one_owner_and_open_never_creates_missing_members() {
    use std::{os::unix::fs::PermissionsExt, sync::Barrier};
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = FakeHost::new();
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let root = temp.path().to_owned();
            let peer = host.peer();
            std::thread::spawn(move || {
                barrier.wait();
                Bridge::create(&root, peer, QualifiedClock::assume_bounded_skew(0).unwrap())
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|v| v.is_ok()).count(), 1);
    drop(results);
    std::fs::rename(
        temp.path().join("owner.lock"),
        temp.path().join("retained.lock"),
    )
    .unwrap();
    assert!(
        Bridge::open(
            temp.path(),
            host.peer(),
            QualifiedClock::assume_bounded_skew(0).unwrap()
        )
        .is_err()
    );
    assert!(!temp.path().join("owner.lock").exists());
}

#[cfg(feature = "test-support")]
#[test]
fn missing_journal_between_temp_fsync_and_rename_poison_refuses_all_effects() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let mut bridge = open(&temp, &host);
    private_file(&temp.path().join("test-before-rename.arm"), b"synthetic");
    let command = envelope(&host, 1, vec![Effect::Input(host.input(10))]);
    let thread = std::thread::spawn(move || {
        let result = bridge.receive(&command);
        (bridge, result)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !temp.path().join("test-before-rename.reached").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::fs::rename(
        temp.path().join("bridge.json"),
        temp.path().join("retained.json"),
    )
    .unwrap();
    private_file(&temp.path().join("test-before-rename.release"), b"release");
    let (mut bridge, result) = thread.join().unwrap();
    assert!(result.is_err());
    std::fs::rename(
        temp.path().join("retained.json"),
        temp.path().join("bridge.json"),
    )
    .unwrap();
    assert!(bridge.recover_pending().is_err());
    assert_eq!(host.state.lock().unwrap().physical, 0);
    drop(bridge);
    assert_eq!(open(&temp, &host).operation_count(), 0);
}

#[test]
fn submitted_local_mailbox_is_observed_without_cancel_or_another_submission() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    host.state.lock().unwrap().pending = true;
    let mut bridge = open(&temp, &host);
    let mut input = host.input(10);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    input.lease.issued_at_ms = now;
    input.lease.expires_at_ms = now + 10000;
    input.lease.execution_expires_at_ms = now + 20000;
    input.issued_at_ms = now;
    input.deadline_ms = now + 2000;
    let prepared = bridge
        .receive(&envelope(&host, 1, vec![Effect::Input(input.clone())]))
        .unwrap()
        .pop()
        .unwrap();
    bridge.execute(prepared).unwrap();
    assert_eq!(bridge.recover_pending().unwrap(), 0);
    drop(bridge);
    let mut bridge = open(&temp, &host);
    assert_eq!(bridge.recover_pending().unwrap(), 0);
    assert_eq!(host.state.lock().unwrap().refused, 0);
    let receipt = Receipt::Input(InputReceipt {
        terminal: input.lease.terminal.clone(),
        operation_id: input.operation_id,
        command_digest: digest(&input),
        disposition: InputDisposition::Applied,
    });
    host.state
        .lock()
        .unwrap()
        .receipts
        .insert(input.operation_id, receipt);
    assert_eq!(bridge.recover_pending().unwrap(), 1);
    assert_eq!(host.state.lock().unwrap().submissions, 1);
}

#[test]
fn bounded_recovery_rotates_past_a_busy_terminal_and_preserves_other_metadata_progress() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    host.state.lock().unwrap().block_receipts = true;
    let mut bridge = open(&temp, &host);
    bridge
        .receive(&envelope(
            &host,
            1,
            (0..MAX_BATCH)
                .map(|n| Effect::Input(host.input(1000 + n as u128)))
                .collect(),
        ))
        .unwrap();
    let mut other = FakeHost::new();
    other.binding.terminal.terminal_id = id(300);
    if let SessionIdentity::Remote {
        attempt_id,
        session_id,
        ..
    } = &mut other.binding.terminal.session
    {
        *attempt_id = id(301);
        *session_id = id(302);
    }
    other.binding.guardian.intent.session = other.binding.terminal.session.clone();
    other.binding.guardian.intent.launch_id = id(303);
    other.binding.guardian.nonce = id(304);
    let other = Arc::new(other);
    bridge.attach(other.clone()).unwrap();
    let registration = bridge
        .prepare_registration(&other.binding.terminal, 1)
        .unwrap();
    bridge.confirm_registration(&registration).unwrap();
    bridge
        .receive(&envelope(&other, 2, vec![Effect::Input(other.input(2000))]))
        .unwrap();
    assert_eq!(bridge.recover_pending().unwrap(), 0);
    assert_eq!(bridge.recover_pending().unwrap(), 1);
    let upload = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(upload.receipts.len(), 1);
    assert_eq!(upload.receipts[0].terminal, other.binding.terminal);
    assert_eq!(host.state.lock().unwrap().physical, 0);
}
