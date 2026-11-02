//! File/CLI vectors only. Fake presence/acks below create no real worker or B1 authority.
use aq_agent_client::completion::ResultClient;
use aq_agent_result_local::{
    self as wire, Acknowledgment, Binding, Conclusion, Phase, Presence, Submission, SubmissionInput,
};
use aq_session_protocol::{ArtifactRef, Digest, ProfiledOffer, Text};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use uuid::Uuid;
struct Fixture {
    dir: tempfile::TempDir,
    binding: Binding,
    owner: Option<File>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(dir.path().join(wire::SUBMISSIONS_DIRECTORY)).unwrap();
        std::fs::set_permissions(
            dir.path().join(wire::SUBMISSIONS_DIRECTORY),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let offer: ProfiledOffer =
            serde_json::from_slice(include_bytes!("fixtures/profiled-offer.json")).unwrap();
        let binding = Binding {
            schema_version: 1,
            launch: offer.binding,
            execution_record_digest: Digest::of(b"fixture only, not actual execution"),
            expected_outcome: ArtifactRef {
                id: Text::new("aq.expected-outcome").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"fixture outcome"),
            },
            operation_id: Uuid::new_v4(),
            report_id: Uuid::new_v4(),
            nonce: Uuid::new_v4(),
        };
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.path().join(wire::OWNER_LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&owner).unwrap();
        let this = Self {
            dir,
            binding,
            owner: Some(owner),
        };
        this.write(wire::BINDING_FILE, &wire::encode(&this.binding).unwrap());
        let stamp = aq_agent_client::journal::Stamp::now().unwrap();
        this.presence(Presence {
            schema_version: 1,
            binding_digest: this.binding.digest().unwrap(),
            kernel_boot_id: stamp.boot_id,
            original_execution_deadline_boot_ns: (stamp.boottime_ms + 10_000) * 1_000_000,
            original_hard_deadline_boot_ns: (stamp.boottime_ms + 20_000) * 1_000_000,
        });
        this
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.dir.path().join(name), bytes).unwrap();
        std::fs::set_permissions(
            self.dir.path().join(name),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    fn presence(&self, presence: Presence) {
        self.write(wire::PRESENCE_FILE, &wire::encode(&presence).unwrap());
    }
    fn client(&self) -> ResultClient {
        ResultClient::open(self.dir.path()).unwrap()
    }
    fn path(&self) -> PathBuf {
        self.dir
            .path()
            .join(wire::SUBMISSIONS_DIRECTORY)
            .join(wire::SUBMISSION_FILE)
    }
    fn input(&self) -> SubmissionInput {
        SubmissionInput {
            schema_version: 1,
            conclusion: Conclusion::new(
                "Changed scoped file; cargo test -p example exit0; limits none",
            )
            .unwrap(),
        }
    }
    fn submission(&self) -> Submission {
        wire::decode(&std::fs::read(self.path()).unwrap()).unwrap()
    }
    fn ack(&self, phase: Phase) -> Acknowledgment {
        let s = self.submission();
        Acknowledgment {
            schema_version: 1,
            operation_id: s.binding.operation_id,
            report_id: s.binding.report_id,
            binding_digest: s.binding.digest().unwrap(),
            submission_digest: s.digest().unwrap(),
            phase,
        }
    }
    fn publish_ack(&self, ack: &Acknowledgment) {
        self.write(wire::ACK_FILE, &wire::encode(ack).unwrap());
    }
}
#[test]
fn one_original_outbox_reopens_and_conflicting_conclusion_never_replaces_it() {
    let f = Fixture::new();
    assert_eq!(f.client().submit(f.input()).unwrap().exit_code, 10);
    let bytes = std::fs::read(f.path()).unwrap();
    let repeat = f.client().submit(f.input()).unwrap();
    assert_eq!(repeat.exit_code, 10);
    assert_eq!(bytes, std::fs::read(f.path()).unwrap());
    let changed = SubmissionInput {
        schema_version: 1,
        conclusion: Conclusion::new("different claim").unwrap(),
    };
    assert_eq!(f.client().submit(changed).unwrap().exit_code, 11);
    assert_eq!(bytes, std::fs::read(f.path()).unwrap());
    assert!(
        !repeat
            .value
            .to_string()
            .contains(&f.binding.nonce.to_string())
    );
}
#[test]
fn missing_owner_original_execution_expiry_and_changed_boot_refuse_new_publication() {
    let mut f = Fixture::new();
    drop(f.owner.take());
    assert_eq!(f.client().submit(f.input()).unwrap().exit_code, 13);
    assert!(!f.path().exists());
    let f = Fixture::new();
    let stamp = aq_agent_client::journal::Stamp::now().unwrap();
    f.presence(Presence {
        schema_version: 1,
        binding_digest: f.binding.digest().unwrap(),
        kernel_boot_id: stamp.boot_id,
        original_execution_deadline_boot_ns: 1,
        original_hard_deadline_boot_ns: (stamp.boottime_ms + 20_000) * 1_000_000,
    });
    assert_eq!(f.client().submit(f.input()).unwrap().exit_code, 13);
    assert!(!f.path().exists());
    let f = Fixture::new();
    let stamp = aq_agent_client::journal::Stamp::now().unwrap();
    f.presence(Presence {
        schema_version: 1,
        binding_digest: f.binding.digest().unwrap(),
        kernel_boot_id: Uuid::new_v4(),
        original_execution_deadline_boot_ns: (stamp.boottime_ms + 10_000) * 1_000_000,
        original_hard_deadline_boot_ns: (stamp.boottime_ms + 20_000) * 1_000_000,
    });
    assert_eq!(f.client().submit(f.input()).unwrap().exit_code, 13);
    assert!(!f.path().exists());
}
#[test]
fn acknowledgment_pins_foreign_bytes_and_owner_loss_cannot_become_live_acceptance() {
    let mut f = Fixture::new();
    f.client().submit(f.input()).unwrap();
    let ack = f.ack(Phase::Accepted);
    f.publish_ack(&ack);
    assert_eq!(
        f.client().status().unwrap().value["phase"]["kind"],
        "accepted"
    );
    let stamp = aq_agent_client::journal::Stamp::now().unwrap();
    f.presence(Presence {
        schema_version: 1,
        binding_digest: f.binding.digest().unwrap(),
        kernel_boot_id: stamp.boot_id,
        original_execution_deadline_boot_ns: 1,
        original_hard_deadline_boot_ns: (stamp.boottime_ms + 20_000) * 1_000_000,
    });
    f.publish_ack(&f.ack(Phase::Finalizing));
    let finalizing = f.client().status().unwrap();
    assert_eq!(finalizing.exit_code, 10);
    assert_eq!(finalizing.value["phase"]["kind"], "finalizing");
    assert_eq!(finalizing.value["owner_present"], true);
    let mut foreign = ack.clone();
    foreign.submission_digest = Digest::of(b"another conclusion");
    f.publish_ack(&foreign);
    assert!(f.client().status().is_err());
    f.publish_ack(&ack);
    drop(f.owner.take());
    let recovered = f.client().status().unwrap();
    assert_eq!(recovered.exit_code, 13);
    assert_eq!(recovered.value["phase"]["kind"], "needs_reconciliation");
    let recorded = f.ack(Phase::Recorded {
        receipt: ArtifactRef {
            id: Text::new(format!("aq.result-record/{}", f.binding.report_id)).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture reference, not actual B1 proof"),
        },
    });
    f.publish_ack(&recorded);
    assert_eq!(
        f.client().status().unwrap().exit_code,
        0,
        "recorded is historical data only"
    );
    assert!(
        !f.client().status().unwrap().value["owner_present"]
            .as_bool()
            .unwrap()
    );
}
#[test]
fn new_publication_waits_for_exact_worker_ack_without_replacing_original() {
    let f = Fixture::new();
    let claim_path = f.path();
    let root = f.dir.path().to_owned();
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        while !claim_path.exists() {
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(5));
        }
        let bytes = std::fs::read(&claim_path).unwrap();
        let s: Submission = wire::decode(&bytes).unwrap();
        // Deliberately delay the synthetic ACK after observing atomic publication.
        std::thread::sleep(Duration::from_millis(50));
        let ack = Acknowledgment {
            schema_version: 1,
            operation_id: s.binding.operation_id,
            report_id: s.binding.report_id,
            binding_digest: s.binding.digest().unwrap(),
            submission_digest: s.digest().unwrap(),
            phase: Phase::Accepted,
        };
        let pending = root.join("ack.pending");
        std::fs::write(&pending, wire::encode(&ack).unwrap()).unwrap();
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::rename(pending, root.join(wire::ACK_FILE)).unwrap();
        bytes
    });
    let result = f.client().submit(f.input()).unwrap();
    assert_eq!(result.exit_code, 10);
    assert_eq!(result.value["phase"]["kind"], "accepted");
    assert_eq!(std::fs::read(f.path()).unwrap(), worker.join().unwrap());
}
#[test]
fn new_publication_wait_cannot_outlive_original_execution_deadline() {
    let f = Fixture::new();
    let stamp = aq_agent_client::journal::Stamp::now().unwrap();
    f.presence(Presence {
        schema_version: 1,
        binding_digest: f.binding.digest().unwrap(),
        kernel_boot_id: stamp.boot_id,
        original_execution_deadline_boot_ns: (stamp.boottime_ms + 500) * 1_000_000,
        original_hard_deadline_boot_ns: (stamp.boottime_ms + 10_000) * 1_000_000,
    });
    let started = Instant::now();
    let result = f.client().submit(f.input()).unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(result.exit_code, 10);
    assert_eq!(result.value["phase"]["kind"], "pending_local");
    assert!(f.path().exists());
    assert_eq!(f.client().submit(f.input()).unwrap().exit_code, 10);
}
#[test]
fn lost_original_with_existing_ack_refuses_replacement_even_with_presence() {
    let f = Fixture::new();
    f.client().submit(f.input()).unwrap();
    f.publish_ack(&f.ack(Phase::Accepted));
    std::fs::remove_file(f.path()).unwrap();
    let result = f.client().submit(f.input()).unwrap();
    assert_eq!(result.exit_code, 13);
    assert!(!f.path().exists());
}
#[test]
fn changed_binding_directory_or_symlink_refuses_before_any_publish() {
    let f = Fixture::new();
    let client = f.client();
    let mut changed = f.binding.clone();
    changed.nonce = Uuid::new_v4();
    f.write(wire::BINDING_FILE, &wire::encode(&changed).unwrap());
    assert!(client.submit(f.input()).is_err());
    assert!(!f.path().exists());
    let f = Fixture::new();
    let copied = f.dir.path().join("copied.json");
    std::fs::copy(f.dir.path().join(wire::BINDING_FILE), &copied).unwrap();
    std::fs::remove_file(f.dir.path().join(wire::BINDING_FILE)).unwrap();
    symlink(&copied, f.dir.path().join(wire::BINDING_FILE)).unwrap();
    assert!(ResultClient::open(f.dir.path()).is_err());
}
#[test]
fn cli_env_channel_needs_no_graph_config_and_missing_channel_never_falls_back() {
    let f = Fixture::new();
    let input = f
        .dir
        .path()
        .join(wire::SUBMISSIONS_DIRECTORY)
        .join("request.json");
    std::fs::write(&input, wire::encode(&f.input()).unwrap()).unwrap();
    let binary = env!("CARGO_BIN_EXE_aq-agent");
    let submitted = Command::new(binary)
        .env(wire::RESULT_DIR_ENV, f.dir.path())
        .args(["result", "submit", "--request-file"])
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(
        submitted.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&submitted.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&submitted.stdout).unwrap();
    assert_eq!(value["operation_id"], f.binding.operation_id.to_string());
    assert!(!value.to_string().contains(&f.binding.nonce.to_string()));
    assert!(!f.dir.path().join("config.json").exists());
    let status = Command::new(binary)
        .env(wire::RESULT_DIR_ENV, f.dir.path())
        .args(["result", "status"])
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(10));
    let absent = Command::new(binary)
        .env_remove(wire::RESULT_DIR_ENV)
        .args(["result", "status"])
        .output()
        .unwrap();
    assert_eq!(absent.status.code(), Some(15));
}
#[test]
fn input_file_is_bounded_regular_and_privilege_fields_are_rejected() {
    let f = Fixture::new();
    let input = f
        .dir
        .path()
        .join(wire::SUBMISSIONS_DIRECTORY)
        .join("request.json");
    std::fs::write(
        &input,
        b"{\"schema_version\":1,\"conclusion\":\"a\",\"stop\":true}",
    )
    .unwrap();
    assert!(aq_agent_client::completion::read_input(&input).is_err());
    let alias = input.with_extension("link");
    symlink(&input, &alias).unwrap();
    assert!(aq_agent_client::completion::read_input(&alias).is_err());
    std::fs::write(&input, vec![b' '; wire::MAX_INPUT_BYTES + 1]).unwrap();
    assert!(aq_agent_client::completion::read_input(&input).is_err());
}
