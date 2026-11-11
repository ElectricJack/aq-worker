//! Finite native source intents. Serialized records are history, never Git proof.
use super::*;
use crate::{
    native::VerifiedArtifact,
    source::{GitAction, GitClient, GitManifest, PendingGitOperation},
};
use aq_admission::{SourceAdmission, execution_spec::ProfiledExecutionSpec};
use aq_worker_protocol::{
    SourceBinding, SourceRole, TransferFile, TransferManifest, Validate as _,
};
use serde_json::{Value, json};
use std::path::PathBuf;
const MAX_RANGES: u32 = (1024 * 1024 * 1024 + 1024 * 1024) / 262144;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub operation_id: Uuid,
    pub file: TransferFile,
    pub offset: u64,
    pub length: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDownload {
    pub spec: ProfiledExecutionSpec,
    pub spec_artifact: ArtifactRef,
    pub preparation: InputOrigin,
    pub created_revision: u64,
    pub created_ns: u64,
    pub root: PathBuf,
    pub reserved_bytes: u64,
    pub discovery_id: Uuid,
    pub manifest: Option<TransferManifest>,
    pub manifest_offset: u64,
    pub bundle_offset: u64,
    pub pending: Option<SourceRange>,
    pub ranges: u32,
    #[serde(
        default = "default_range_bytes",
        skip_serializing_if = "is_default_range"
    )]
    pub range_bytes: u32,
    pub verified: Option<GitManifest>,
    pub operations: BTreeMap<String, PendingGitOperation>,
}
fn default_range_bytes() -> u32 {
    262144
}
fn is_default_range(value: &u32) -> bool {
    *value == default_range_bytes()
}
impl SourceDownload {
    pub fn admission(&self) -> &SourceAdmission {
        &self.spec.source
    }
    pub fn binding(&self) -> Result<&SourceBinding> {
        Ok(&self
            .manifest
            .as_ref()
            .context("source manifest absent")?
            .source)
    }
    pub fn complete(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| {
            self.manifest_offset == m.manifest_bytes && self.bundle_offset == m.bundle_bytes
        })
    }
}
fn transfer(s: &SourceDownload, m: &TransferManifest, attempt: &Attempt) -> Result<()> {
    m.validate()?;
    let b = &attempt.reservation.offer.offered.offer.binding;
    let SessionIdentity::Remote { session_id, .. } = b.session else {
        anyhow::bail!("source remote binding")
    };
    ensure!(
        m.role == SourceRole::Base
            && m.transfer_id == m.artifact_id
            && m.bundle_bytes > 0
            && m.manifest_bytes <= 1_048_576
            && m.manifest_bytes
                .div_ceil(u64::from(s.range_bytes))
                .checked_add(m.bundle_bytes.div_ceil(u64::from(s.range_bytes)))
                .is_some_and(|n| n <= u64::from(MAX_RANGES))
            && m.attempt.attempt_id == b.attempt_id
            && m.attempt.session_id == session_id
            && m.attempt.generation == b.assignment_generation
            && m.resolved_spec_digest == b.execution_spec_digest.as_str()
            && m.source.repository_id == s.spec.source.repository_id
            && m.source.binding_generation == s.spec.source.repository_binding_generation
            && m.source.base.hex == s.spec.source.resolved_object.hex
            && m.checkpoint == m.source.base,
        "native source manifest association"
    );
    Ok(())
}
fn request(s: &SourceDownload, key: &str, id: Uuid) -> Result<Value> {
    let b = s.binding()?;
    Ok(match key {
        "import" => GitAction::Import {
            artifact_handle: s.manifest.as_ref().unwrap().artifact_id,
        }
        .request(b, id),
        "create" => GitAction::Create.request(b, id),
        "repository" | "retention" | "workspace" => {
            let (operation, params) = match key {
                "repository" => ("repo.show", json!({})),
                "retention" => (
                    "retention.inspect",
                    json!({"handle_id":s.operations.get("import").and_then(|p|p.result.as_ref()).context("import result missing")?["retention_handle"],"limit":1}),
                ),
                _ => (
                    "workspace.inspect",
                    json!({"workspace_id":s.operations.get("create").and_then(|p|p.result.as_ref()).context("create result missing")?["workspace_id"]}),
                ),
            };
            json!({"protocol_version":1,"request_id":id,"operation":operation,"repository_id":b.repository_id,"expected_binding_generation":b.binding_generation,"params":params})
        }
        _ => anyhow::bail!("unsupported native source operation"),
    })
}
pub(super) fn validate(a: &Attempt, revision: u64, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    let Some(s) = &a.source else { return Ok(()) };
    s.preparation
        .validate(a, s.created_revision, s.created_ns, revision)?;
    s.spec
        .validate_against(&a.reservation.offer.offered.offer)?;
    ensure!(
        s.spec.artifact()? == s.spec_artifact
            && s.root.is_absolute()
            && s.reserved_bytes > 0
            && s.reserved_bytes <= a.reservation.disk_bytes
            && !s.discovery_id.is_nil()
            && ids.insert(s.discovery_id)
            && s.ranges <= MAX_RANGES
            && matches!(s.range_bytes, 262144 | 1048576)
            && s.operations.len() <= 5,
        "native source plan bounds"
    );
    let input = a
        .downloads
        .values()
        .find(|d| d.descriptor.artifact == s.spec_artifact)
        .context("source spec download absent")?;
    ensure!(
        input.receipt.is_some() && input.created_revision <= revision,
        "source spec not verified"
    );
    if let Some(m) = &s.manifest {
        transfer(s, m, a)?;
        ensure!(
            m.manifest_bytes
                .checked_add(m.bundle_bytes)
                .is_some_and(|n| n <= s.reserved_bytes)
                && s.manifest_offset <= m.manifest_bytes
                && s.bundle_offset <= m.bundle_bytes
                && (s.bundle_offset == 0 || s.manifest_offset == m.manifest_bytes)
                && (s.manifest_offset == m.manifest_bytes
                    || s.manifest_offset % u64::from(s.range_bytes) == 0)
                && (s.bundle_offset == m.bundle_bytes
                    || s.bundle_offset % u64::from(s.range_bytes) == 0)
                && u64::from(s.ranges)
                    == s.manifest_offset.div_ceil(u64::from(s.range_bytes))
                        + s.bundle_offset.div_ceil(u64::from(s.range_bytes))
                        + u64::from(s.pending.is_some()),
            "source cursor bounds"
        );
    } else {
        ensure!(
            s.manifest_offset == 0
                && s.bundle_offset == 0
                && s.ranges == 0
                && s.pending.is_none()
                && s.verified.is_none()
                && s.operations.is_empty(),
            "source before discovery"
        );
    }
    if let Some(r) = &s.pending {
        let m = s.manifest.as_ref().context("range without manifest")?;
        let (offset, size) = match r.file {
            TransferFile::Manifest => (s.manifest_offset, m.manifest_bytes),
            TransferFile::Bundle => (s.bundle_offset, m.bundle_bytes),
        };
        ensure!(
            !r.operation_id.is_nil()
                && ids.insert(r.operation_id)
                && r.offset == offset
                && (1..=s.range_bytes).contains(&r.length)
                && r.offset < size
                && u64::from(r.length) == (size - r.offset).min(u64::from(s.range_bytes))
                && s.ranges > 0,
            "source pending range"
        );
    }
    if let Some(native) = &s.verified {
        let m = s.manifest.as_ref().context("verified without manifest")?;
        ensure!(
            s.complete()
                && s.pending.is_none()
                && native.format_version == 1
                && native.repository_id == m.source.repository_id
                && native.repository_identity == m.source.logical_identity
                && native.binding_generation == m.source.binding_generation
                && native.transfer_id == m.artifact_id
                && native.object_format == "sha1"
                && native.base == m.source.base
                && native.source == m.checkpoint
                && native.bundle_size == m.bundle_bytes
                && native.bundle_sha256 == m.bundle_digest
                && !native.reference_only,
            "source verified associations"
        );
    }
    for (key, op) in &s.operations {
        ensure!(
            s.verified.is_some()
                && !op.operation_id.is_nil()
                && ids.insert(op.operation_id)
                && op.request == request(s, key, op.operation_id)?,
            "native Git immutable request"
        );
        if let Some(v) = &op.result {
            ensure!(
                serde_json::to_vec(v)?.len() <= 32768,
                "native Git result bound"
            );
        }
    }
    Ok(())
}
impl Journal {
    pub(crate) fn begin_source(
        &mut self,
        id: Uuid,
        spec: &VerifiedArtifact,
        git: &GitClient,
    ) -> Result<SourceDownload> {
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let p = InputOrigin::capture(a)?;
        ensure!(now < p.deadline_ns(), "source preparation expired");
        let parsed = aq_admission::execution_spec::decode_profiled_spec(spec.bytes())?;
        parsed.validate_retained(
            &a.reservation.offer.offered.offer,
            &a.reservation.offer.offered.artifacts,
            p.observed_at_ms(),
        )?;
        ensure!(
            parsed.canonical_bytes()? == spec.bytes()
                && spec.receipt().artifact == parsed.artifact()?
                && a.downloads.values().any(|d| d
                    .receipt
                    .as_ref()
                    .is_some_and(|r| r.artifact == spec.receipt().artifact)),
            "source spec is not verified durable original"
        );
        let bytes = git.native_reservation_bytes()?;
        ensure!(
            bytes <= a.reservation.disk_bytes,
            "native source storage reservation insufficient"
        );
        if let Some(old) = &a.source {
            ensure!(
                old.spec_artifact == parsed.artifact()?
                    && old.root == git.root()
                    && old.reserved_bytes == bytes,
                "source owner changed"
            );
            return Ok(old.clone());
        }
        let plan = SourceDownload {
            spec: parsed,
            spec_artifact: spec.receipt().artifact,
            preparation: p.clone(),
            created_revision: self.state.revision,
            created_ns: now,
            root: git.root().to_owned(),
            reserved_bytes: bytes,
            discovery_id: Uuid::new_v4(),
            manifest: None,
            manifest_offset: 0,
            bundle_offset: 0,
            pending: None,
            ranges: 0,
            range_bytes: default_range_bytes(),
            verified: None,
            operations: BTreeMap::new(),
        };
        let mut next = self.next()?;
        next.attempts.get_mut(&id).unwrap().source = Some(plan.clone());
        self.persist(next)?;
        Ok(plan)
    }
    pub(crate) fn source_manifest(
        &mut self,
        id: Uuid,
        m: TransferManifest,
        maximum_range: u32,
    ) -> Result<()> {
        self.source_update(id, |s| {
            if let Some(old) = &s.manifest {
                ensure!(old == &m, "source discovery changed")
            } else {
                ensure!(
                    matches!(maximum_range, 262144 | 1048576),
                    "source range capability"
                );
                s.range_bytes = maximum_range;
                s.manifest = Some(m)
            }
            Ok(())
        })
    }
    pub(crate) fn source_range(&mut self, id: Uuid) -> Result<SourceRange> {
        let mut result = None;
        self.source_update(id, |s| {
            if let Some(old) = &s.pending {
                result = Some(old.clone());
                return Ok(());
            }
            let m = s.manifest.as_ref().context("source discovery absent")?;
            let (file, offset, size) = if s.manifest_offset < m.manifest_bytes {
                (TransferFile::Manifest, s.manifest_offset, m.manifest_bytes)
            } else {
                (TransferFile::Bundle, s.bundle_offset, m.bundle_bytes)
            };
            ensure!(
                offset < size && s.ranges < MAX_RANGES,
                "source range capacity or complete"
            );
            let r = SourceRange {
                operation_id: Uuid::new_v4(),
                file,
                offset,
                length: (size - offset).min(u64::from(s.range_bytes)) as u32,
            };
            s.ranges += 1;
            s.pending = Some(r.clone());
            result = Some(r);
            Ok(())
        })?;
        Ok(result.unwrap())
    }
    pub(crate) fn source_range_synced(&mut self, id: Uuid, r: &SourceRange) -> Result<()> {
        self.source_update(id, |s| {
            let old = s.pending.as_ref().context("source range absent")?;
            ensure!(same(old, r)?, "source range changed");
            match r.file {
                TransferFile::Manifest => s.manifest_offset += u64::from(r.length),
                TransferFile::Bundle => s.bundle_offset += u64::from(r.length),
            };
            s.pending = None;
            Ok(())
        })
    }
    pub(crate) fn source_verified(&mut self, id: Uuid, m: GitManifest) -> Result<()> {
        self.source_update(id, |s| {
            s.verified = Some(m);
            Ok(())
        })
    }
    pub(crate) fn source_operation(&mut self, id: Uuid, key: &str) -> Result<PendingGitOperation> {
        let mut result = None;
        self.source_update(id, |s| {
            if let Some(old) = s.operations.get(key) {
                result = Some(old.clone());
                return Ok(());
            }
            ensure!(s.verified.is_some(), "source files not verified");
            let operation_id = Uuid::new_v4();
            let op = PendingGitOperation {
                operation_id,
                request: request(s, key, operation_id)?,
                result: None,
            };
            s.operations.insert(key.into(), op.clone());
            result = Some(op);
            Ok(())
        })?;
        Ok(result.unwrap())
    }
    pub(crate) fn source_result(&mut self, id: Uuid, key: &str, result: Value) -> Result<()> {
        self.source_update(id, |s| {
            let op = s
                .operations
                .get_mut(key)
                .context("source operation not durable")?;
            if let Some(old) = &op.result
                && matches!(key, "import" | "create")
            {
                ensure!(old == &result, "native Git operation replay changed");
                return Ok(());
            }
            op.result = Some(result);
            Ok(())
        })
    }
    fn source_update(
        &mut self,
        id: Uuid,
        change: impl FnOnce(&mut SourceDownload) -> Result<()>,
    ) -> Result<()> {
        let now = self.live()?;
        let mut next = self.next()?;
        let s = next
            .attempts
            .get_mut(&id)
            .context("unknown native attempt")?
            .source
            .as_mut()
            .context("source plan absent")?;
        ensure!(
            now < s.preparation.deadline_ns(),
            "source preparation expired"
        );
        change(s)?;
        if same(&next.attempts[&id].source, &self.state.attempts[&id].source)? {
            return Ok(());
        }
        self.persist(next)
    }
}
