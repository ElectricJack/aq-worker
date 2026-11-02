//! One bounded local outbox. This module creates no execution, node/Graph,
//! stopped-writer, source-retention, review or task-completion authority.
use crate::{client::Outcome, files::PrivateDir};
use anyhow::{Context, Result, ensure};
use aq_agent_result_local::{
    self as wire, Acknowledgment, Binding, Phase, Presence, Submission, SubmissionInput,
};
use serde_json::json;
use std::{
    fs::{File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
    time::Duration,
};
use uuid::Uuid;

pub struct ResultClient {
    root: PrivateDir,
    submissions: PrivateDir,
    binding: Binding,
    binding_file: File,
    binding_metadata: Metadata,
    binding_bytes: Vec<u8>,
}
impl ResultClient {
    /// Only opens an actually installed channel. Never provisions a directory,
    /// grant, control lock or replacement binding from agent input.
    pub fn open(root: &Path) -> Result<Self> {
        let root = PrivateDir::open(root)?;
        let mut binding_file = root.existing(wire::BINDING_FILE)?;
        let binding_metadata = binding_file.metadata()?;
        ensure!(
            binding_metadata.len() <= wire::MAX_BYTES as u64,
            "result binding byte bound"
        );
        let mut binding_bytes = vec![];
        Read::by_ref(&mut binding_file)
            .take(wire::MAX_BYTES as u64 + 1)
            .read_to_end(&mut binding_bytes)?;
        let binding: Binding = wire::decode(&binding_bytes)?;
        let submissions = root.child(wire::SUBMISSIONS_DIRECTORY)?;
        let result = Self {
            root,
            submissions,
            binding,
            binding_file,
            binding_metadata,
            binding_bytes,
        };
        result.check_binding()?;
        Ok(result)
    }
    fn check_binding(&self) -> Result<()> {
        self.root.check_route()?;
        self.submissions.check_route()?;
        let held = self.binding_file.metadata()?;
        let named = self.root.existing(wire::BINDING_FILE)?.metadata()?;
        ensure!(
            same_file(&held, &self.binding_metadata)
                && same_file(&named, &self.binding_metadata)
                && self.root.read(wire::BINDING_FILE, wire::MAX_BYTES)? == self.binding_bytes,
            "result binding or route changed"
        );
        Ok(())
    }
    fn existing_submission(&self) -> Result<Option<Submission>> {
        self.submissions
            .optional(wire::SUBMISSION_FILE, wire::MAX_BYTES)?
            .map(|bytes| {
                let value: Submission = wire::decode(&bytes)?;
                ensure!(
                    value.binding == self.binding,
                    "result submission belongs to another scope"
                );
                Ok(value)
            })
            .transpose()
    }
    fn acknowledgment(&self, submission: &Submission) -> Result<Option<Acknowledgment>> {
        self.root
            .optional(wire::ACK_FILE, wire::MAX_BYTES)?
            .map(|bytes| {
                let ack: Acknowledgment = wire::decode(&bytes)?;
                ack.validate_for(submission)?;
                Ok(ack)
            })
            .transpose()
    }
    fn owner_present(&self, new_submission: bool) -> Result<bool> {
        let Some(bytes) = self.root.optional(wire::PRESENCE_FILE, wire::MAX_BYTES)? else {
            return Ok(false);
        };
        let presence: Presence = wire::decode(&bytes)?;
        let (boot, now) = kernel_clock()?;
        let matches = if new_submission {
            presence.matches_at(&self.binding, boot, now)?
        } else {
            presence.matches_owner_projection(&self.binding, boot)?
        };
        if !matches {
            return Ok(false);
        }
        // Existing lock only. Acquiring a lock here would be owner ABSENCE,
        // never permission to act as the owner. No PID JSON is trusted.
        let lock = match self.root.existing(wire::OWNER_LOCK_FILE) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        match fs2::FileExt::try_lock_shared(&lock) {
            Ok(()) => {
                fs2::FileExt::unlock(&lock)?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
    fn view(
        &self,
        submission: Option<&Submission>,
        ack: Option<&Acknowledgment>,
        present: bool,
    ) -> Result<Outcome> {
        let (phase, exit_code) = match ack {
            Some(ack) if matches!(ack.phase, Phase::Recorded { .. }) => {
                (serde_json::to_value(&ack.phase)?, 0)
            }
            Some(ack) if matches!(ack.phase, Phase::NeedsReconciliation { .. }) => {
                (serde_json::to_value(&ack.phase)?, 13)
            }
            Some(ack) if present => (serde_json::to_value(&ack.phase)?, 10),
            _ if !present => (
                json!({"kind":"needs_reconciliation","code":"owner_unavailable"}),
                13,
            ),
            _ if submission.is_some() => (json!({"kind":"pending_local"}), 10),
            _ => (json!({"kind":"ready_for_claim"}), 0),
        };
        Ok(Outcome {
            value: json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id,
            "binding_digest":self.binding.digest()?,"expected_outcome":self.binding.expected_outcome,
            "submission_digest":submission.map(Submission::digest).transpose()?,"phase":phase,
            "owner_present":present,"projection":"local_result_data_only"}),
            exit_code,
        })
    }
    pub fn status(&self) -> Result<Outcome> {
        self.check_binding()?;
        let submission = self.existing_submission()?;
        let ack = submission
            .as_ref()
            .map(|s| self.acknowledgment(s))
            .transpose()?
            .flatten();
        // A worker ack without its retained original local claim is not a
        // replacement claim or proof that an absent file had no prior effect.
        ensure!(
            submission.is_some()
                || self
                    .root
                    .optional(wire::ACK_FILE, wire::MAX_BYTES)?
                    .is_none(),
            "result acknowledgment has no retained original claim"
        );
        // Without an original claim, Ready must still mean the original
        // execution ceiling permits publication. A retained claim may continue
        // finalizing under its completion owner after that ceiling expires.
        let present = self.owner_present(submission.is_none())?;
        self.check_binding()?;
        self.view(submission.as_ref(), ack.as_ref(), present)
    }
    pub fn submit(&self, input: SubmissionInput) -> Result<Outcome> {
        self.check_binding()?;
        let candidate = Submission::new(self.binding.clone(), input)?;
        let _writer = self.submissions.lock_named(wire::WRITER_LOCK_FILE)?;
        if let Some(existing) = self.existing_submission()? {
            if existing != candidate {
                return Ok(Outcome {
                    value: json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id,"code":"result_claim_conflict","effect":"original_preserved","retryable":false}),
                    exit_code: 11,
                });
            }
            return self.status();
        }
        if self
            .root
            .optional(wire::ACK_FILE, wire::MAX_BYTES)?
            .is_some()
        {
            return Ok(Outcome {
                value: json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id,"phase":{"kind":"needs_reconciliation","code":"storage_uncertain"},"projection":"local_result_data_only"}),
                exit_code: 13,
            });
        }
        if !self.owner_present(true)? {
            return self.view(None, None, false);
        }
        self.check_binding()?;
        let bytes = wire::encode(&candidate)?;
        if let Err(error) = self.submissions.write_new(wire::SUBMISSION_FILE, &bytes) {
            // A competing/uncertain publication never receives a new UUID or
            // overwrite. Keep this original scope available for status/recovery.
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
            {
                if self.existing_submission()?.as_ref() == Some(&candidate) {
                    return self.status();
                }
                return Ok(Outcome {
                    value: json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id,"code":"result_claim_conflict","effect":"original_preserved","retryable":false}),
                    exit_code: 11,
                });
            }
            return self.unknown(&candidate);
        }
        if self.check_binding().is_err() {
            return self.unknown(&candidate);
        }
        // Publication can race cancellation/expiry. Only the actual worker may
        // accept it; do not manufacture an Accepted acknowledgment here.
        self.wait_for_acknowledgment()
    }
    /// One bounded local wait after NEW publication only. It never retries,
    /// replaces an intent or renews its original execution ceiling. Pending on
    /// timeout is honest uncertainty; only worker-published matching ACKs count.
    fn wait_for_acknowledgment(&self) -> Result<Outcome> {
        let presence: Presence =
            wire::decode(&self.root.read(wire::PRESENCE_FILE, wire::MAX_BYTES)?)?;
        let (boot, now) = kernel_clock()?;
        if !presence.matches_at(&self.binding, boot, now)? {
            return self.status();
        }
        let until = presence
            .original_execution_deadline_boot_ns
            .min(now.saturating_add(2_000_000_000));
        loop {
            let outcome = self.status()?;
            if outcome.value["phase"]["kind"] != "pending_local" {
                return Ok(outcome);
            }
            let (current_boot, now) = kernel_clock()?;
            if current_boot != boot || now >= until {
                return Ok(outcome);
            }
            std::thread::sleep(Duration::from_nanos((until - now).min(20_000_000)));
        }
    }
    fn unknown(&self, submission: &Submission) -> Result<Outcome> {
        Ok(Outcome {
            value: json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id,"submission_digest":submission.digest()?,"code":"local_submission_unknown","effect":"possible","retryable":false}),
            exit_code: 13,
        })
    }
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn kernel_clock() -> Result<(Uuid, u64)> {
    let mut boot = vec![];
    File::open("/proc/sys/kernel/random/boot_id")?
        .take(65)
        .read_to_end(&mut boot)?;
    ensure!(boot.len() <= 64, "kernel boot identity bound");
    let boot = std::str::from_utf8(&boot)?.trim().parse()?;
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0
            && time.tv_sec >= 0
            && time.tv_nsec >= 0
            && time.tv_nsec < 1_000_000_000,
        "boottime unavailable"
    );
    let now = (time.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(time.tv_nsec as u64))
        .context("boottime overflow")?;
    Ok((boot, now))
}

/// Untrusted input is a bounded regular file, not a FIFO/symlink or an arbitrary
/// transport. Its bytes never select framework operations or credentials.
pub fn read_input(path: &Path) -> Result<SubmissionInput> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.len() <= wire::MAX_INPUT_BYTES as u64,
        "result request file shape/bound"
    );
    let mut bytes = vec![];
    file.take(wire::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(wire::decode_input(&bytes)?)
}
