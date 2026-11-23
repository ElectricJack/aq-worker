use super::*;
use std::collections::BTreeMap;
mod publication;
pub use publication::CandidatePublishedInputs;

pub const PROFILE: &str = "linux-x86_64-prepared-static-capsule-candidate-v1";
pub(super) fn digest(value: &impl Serialize) -> Result<Digest> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(Digest::of(&serde_json::to_vec(&value)?))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedSpec {
    pub owner_intent: ArtifactRef,
    pub preparation_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidatePreparedInfo {
    owner_intent: ArtifactRef,
    preparation_digest: Digest,
    capsule_id: Uuid,
    original_deadline_boot_ms: u64,
    guardian: ProcessEvidence,
    namespace_init: ProcessEvidence,
    exec_stub: ProcessEvidence,
    origin: String,
    argument_prefix: Vec<String>,
    auth_directory: (u64, u64),
}
impl CandidatePreparedInfo {
    pub fn owner_intent(&self) -> &ArtifactRef {
        &self.owner_intent
    }
    pub fn preparation_digest(&self) -> &Digest {
        &self.preparation_digest
    }
    pub fn capsule_id(&self) -> Uuid {
        self.capsule_id
    }
    pub fn deadline_boot_ms(&self) -> u64 {
        self.original_deadline_boot_ms
    }
    pub fn guardian(&self) -> &ProcessEvidence {
        &self.guardian
    }
    pub fn namespace_init(&self) -> &ProcessEvidence {
        &self.namespace_init
    }
    pub fn exec_stub(&self) -> &ProcessEvidence {
        &self.exec_stub
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
}
pub(super) fn preparation_digest(plan: &PlanRecord, owner: &ArtifactRef) -> Result<Digest> {
    let mut plan = plan.clone();
    plan.prepared = None;
    digest(&(PROFILE, owner, plan))
}
pub(super) fn validate_plan(plan: &PlanRecord, files: &[File]) -> Result<()> {
    let Some(spec) = &plan.prepared else {
        return Ok(());
    };
    spec.owner_intent.id.validate()?;
    spec.owner_intent.version.validate()?;
    ensure!(
        spec.preparation_digest == preparation_digest(plan, &spec.owner_intent)?
            && !plan.arguments.iter().any(|a| a.starts_with("--candidate-")),
        "prepared plan digest/prefix"
    );
    let index = plan
        .grants
        .iter()
        .position(|g| g.target == MountTarget::DerivedAuth)
        .context("prepared auth grant missing")?;
    let directory = &files[index + 3];
    let meta = directory.metadata()?;
    ensure!(
        !plan.grants[index].writable
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o7777 == 0o700,
        "prepared private auth grant"
    );
    ensure!(
        std::fs::read_dir(format!("/proc/thread-self/fd/{}", directory.as_raw_fd()))?
            .next()
            .is_none(),
        "prepared auth directory must be fresh"
    );
    Ok(())
}
pub(super) struct PreparedProcesses {
    pub(super) guardian: ProcessEvidence,
    pub(super) init: ProcessEvidence,
    pub(super) stub: ProcessEvidence,
}

pub(super) fn info(
    plan: &PlanRecord,
    processes: PreparedProcesses,
    origin: String,
) -> Result<CandidatePreparedInfo> {
    let spec = plan.prepared.as_ref().context("prepared mode absent")?;
    let grant = plan
        .grants
        .iter()
        .find(|g| g.target == MountTarget::DerivedAuth)
        .context("prepared auth target")?;
    Ok(CandidatePreparedInfo {
        owner_intent: spec.owner_intent.clone(),
        preparation_digest: spec.preparation_digest.clone(),
        capsule_id: plan.nonce,
        original_deadline_boot_ms: plan.deadline,
        guardian: processes.guardian,
        namespace_init: processes.init,
        exec_stub: processes.stub,
        origin,
        argument_prefix: plan.arguments.clone(),
        auth_directory: (grant.pin.device, grant.pin.inode),
    })
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateStaticRecipe {
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
}
impl CandidateStaticRecipe {
    pub(super) fn into_parts(self) -> (Vec<String>, Vec<(String, String)>) {
        (self.arguments, self.environment.into_iter().collect())
    }
    pub fn new(info: &CandidatePreparedInfo) -> Self {
        let mut arguments = info.argument_prefix.clone();
        arguments.extend([
            format!("--candidate-origin={}", info.origin),
            format!(
                "--candidate-preparation={}",
                info.preparation_digest.as_str()
            ),
            format!("--candidate-deadline={}", info.original_deadline_boot_ms),
        ]);
        Self {
            arguments,
            environment: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("AQ_CAPSULE_ENDPOINT".into(), info.origin.clone()),
            ]),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitRecord {
    scope: CandidatePreparedInfo,
    published: CandidatePublishedInputs,
    recipe: CandidateStaticRecipe,
    transport_scope_digest: Digest,
    digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateCommit(CommitRecord);
fn commit_digest(record: &CommitRecord) -> Result<Digest> {
    digest(&(
        &record.scope,
        &record.published,
        &record.recipe,
        &record.transport_scope_digest,
    ))
}
impl CandidateCommit {
    pub fn new(
        info: &CandidatePreparedInfo,
        published: CandidatePublishedInputs,
        recipe: CandidateStaticRecipe,
        transport_scope_digest: Digest,
    ) -> Result<Self> {
        let mut record = CommitRecord {
            scope: info.clone(),
            published,
            recipe,
            transport_scope_digest,
            digest: Digest::of(b"not-self-referential"),
        };
        record.digest = commit_digest(&record)?;
        validate_record(&record, info)?;
        Ok(Self(record))
    }
    pub fn digest(&self) -> &Digest {
        &self.0.digest
    }
}
pub(super) fn validate_record(
    record: &CommitRecord,
    expected: &CandidatePreparedInfo,
) -> Result<()> {
    ensure!(
        record.scope == *expected
            && record.digest == commit_digest(record)?
            && record.recipe == CandidateStaticRecipe::new(expected),
        "prepared exact commit scope/digest/recipe"
    );
    ensure!(
        ownership::raw_boot_ms() < expected.original_deadline_boot_ms,
        "prepared commit expired"
    );
    Ok(())
}
pub(super) fn verify_exec(
    plan: &PlanRecord,
    port: u16,
    record: &CommitRecord,
) -> Result<CandidateStaticRecipe> {
    let scope = &record.scope;
    verify_scope(plan, port, scope)?;
    validate_record(record, scope)?;
    record.published.verify(scope)?;
    Ok(record.recipe.clone())
}
fn verify_scope(plan: &PlanRecord, port: u16, scope: &CandidatePreparedInfo) -> Result<()> {
    let spec = plan
        .prepared
        .as_ref()
        .context("prepared commit on legacy mode")?;
    ensure!(
        scope.owner_intent == spec.owner_intent
            && scope.preparation_digest == spec.preparation_digest
            && scope.capsule_id == plan.nonce
            && scope.original_deadline_boot_ms == plan.deadline
            && scope.origin == format!("http://127.0.0.1:{port}")
            && scope.argument_prefix == plan.arguments,
        "prepared stub exact identity"
    );
    Ok(())
}

pub struct CandidatePreparedStopReport {
    info: CandidatePreparedInfo,
    reaped: u64,
}
impl CandidatePreparedStopReport {
    pub fn info(&self) -> &CandidatePreparedInfo {
        &self.info
    }
    pub fn kernel_echild(&self) -> bool {
        true
    }
    pub fn reaped_count(&self) -> u64 {
        self.reaped
    }
}
pub struct CandidatePreparedHandle {
    owner: ChildOwner,
    info: CandidatePreparedInfo,
    listener: Option<TcpListener>,
    program: File,
}
/// Owns an uncertain/failed commit until its caller has fenced external work.
/// No retry, stopped receipt, or prepared-abort claim can be derived from it.
pub struct CandidateCommitFailure {
    _owner: ChildOwner,
    _program: File,
    error: anyhow::Error,
}
impl CandidateCommitFailure {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    fn into_error(self) -> anyhow::Error {
        self.error
    }
}
impl CandidatePreparedHandle {
    pub fn prepare(
        helper: &PinnedHelper,
        mut plan: StaticCapsulePlan,
        owner_intent: ArtifactRef,
    ) -> Result<Self> {
        ensure!(
            plan.record.prepared.is_none(),
            "prepared plan already selected"
        );
        let preparation_digest = preparation_digest(&plan.record, &owner_intent)?;
        plan.record.prepared = Some(PreparedSpec {
            owner_intent,
            preparation_digest,
        });
        validate_plan(&plan.record, &plan.files)?;
        let (owner, guardian_pid) = spawn_guardian(helper, &plan)?;
        let packet = owner
            .control
            .as_ref()
            .unwrap()
            .recv::<Message>(plan.record.deadline)?;
        ensure!(
            packet.credentials.pid as u32 == guardian_pid,
            "prepared guardian sender"
        );
        let Message::Prepared(info) = packet.value else {
            bail!("prepared stage missing")
        };
        let expected = super::prepared::info(
            &plan.record,
            PreparedProcesses {
                guardian: info.guardian.clone(),
                init: info.namespace_init.clone(),
                stub: info.exec_stub.clone(),
            },
            info.origin.clone(),
        )?;
        ensure!(
            info == expected && info.guardian.pid == guardian_pid,
            "prepared scope mismatch"
        );
        let meta = helper.file.metadata()?;
        let pin = seal::Pin {
            digest: Digest::of(b"metadata-only; sealed identity checked"),
            bytes: meta.len(),
            device: meta.dev(),
            inode: meta.ino(),
            executable: true,
        };
        guardian::verify_image(&info.exec_stub, &pin)?;
        let listener =
            guardian::listener(packet.fd.context("prepared listener absent")?, &info.origin)?;
        let program = plan.files.into_iter().next().unwrap();
        Ok(Self {
            owner,
            info,
            listener: Some(listener),
            program,
        })
    }
    pub fn info(&self) -> &CandidatePreparedInfo {
        &self.info
    }
    pub fn take_listener(&mut self) -> Result<TcpListener> {
        self.listener
            .take()
            .context("prepared listener already taken")
    }
    pub fn commit(self, commit: CandidateCommit) -> Result<CapsuleHandle> {
        self.commit_retaining(commit)
            .map_err(CandidateCommitFailure::into_error)
    }
    /// A failure retains physical ownership, including after a possibly sent
    /// release. Fence and await external work before dropping that owner.
    pub fn commit_retaining(
        self,
        commit: CandidateCommit,
    ) -> std::result::Result<CapsuleHandle, CandidateCommitFailure> {
        match self.finalize(commit) {
            Ok(witness) => Ok(CapsuleHandle {
                owner: self.owner,
                witness,
                listener: None,
                deadline: self.info.original_deadline_boot_ms,
                _program: self.program,
            }),
            Err(error) => Err(CandidateCommitFailure {
                _owner: self.owner,
                _program: self.program,
                error,
            }),
        }
    }
    fn finalize(&self, commit: CandidateCommit) -> Result<CapsuleWitness> {
        ensure!(
            self.listener.is_none(),
            "prepared listener must be handed off before commit"
        );
        validate_record(&commit.0, &self.info)?;
        let socket = self.owner.control.as_ref().unwrap();
        socket.send(
            &Message::Commit(commit.0),
            None,
            self.info.original_deadline_boot_ms,
        )?;
        let packet = socket.recv::<Message>(self.info.original_deadline_boot_ms)?;
        ensure!(
            packet.credentials.pid as u32 == self.info.guardian.pid && packet.fd.is_none(),
            "prepared running sender"
        );
        let Message::Ready(witness) = packet.value else {
            bail!("prepared commit refused or execution uncertain")
        };
        ensure!(
            witness.guardian == self.info.guardian
                && witness.init == self.info.namespace_init
                && witness.harness == self.info.exec_stub
                && witness.nonce == self.info.capsule_id
                && witness.origin == self.info.origin,
            "prepared running identity mismatch"
        );
        let metadata = self.program.metadata()?;
        guardian::verify_image(
            &witness.harness,
            &seal::Pin {
                digest: Digest::of(b"retained sealed identity"),
                bytes: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
                executable: true,
            },
        )?;
        Ok(witness)
    }
    pub fn abort(mut self) -> Result<CandidatePreparedStopReport> {
        self.listener.take();
        let until = (ownership::raw_boot_ms() + CLEANUP_MS)
            .min(self.info.original_deadline_boot_ms + CLEANUP_MS);
        let socket = self.owner.control.as_ref().unwrap();
        let _ = socket.send(
            &Message::Stop {
                nonce: self.info.capsule_id,
            },
            None,
            until,
        );
        let packet = socket.recv::<Message>(until)?;
        ensure!(
            packet.credentials.pid as u32 == self.info.guardian.pid && packet.fd.is_none(),
            "prepared abort sender"
        );
        let Message::PreparedStopped { info, reaped } = packet.value else {
            bail!("prepared stop uncertain")
        };
        ensure!(info == self.info && reaped > 0, "prepared stop binding");
        let _ = socket.send(
            &Message::StoppedAck {
                nonce: self.info.capsule_id,
            },
            None,
            until,
        );
        self.owner.control.take();
        self.owner.wait(until)?;
        Ok(CandidatePreparedStopReport { info, reaped })
    }
}

#[cfg(test)]
mod tests;
