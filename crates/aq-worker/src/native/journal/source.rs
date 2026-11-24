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
    /// The attempt's worktree of the node's repository; no bundle (spec 0016 §5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
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
    pub(crate) fn worktree_path(&self) -> Option<&PathBuf> {
        self.worktree.as_ref()
    }
    fn worktree_matches(&self, attempt_id: Uuid) -> bool {
        self.worktree_path().is_none_or(|path| {
            self.manifest.is_some()
                && self.ranges == 0
                && *path == self.root.join("worktrees").join(attempt_id.to_string())
        })
    }
    fn undiscovered(&self) -> bool {
        (self.manifest_offset, self.bundle_offset, self.ranges) == (0, 0, 0)
            && self.pending.is_none()
            && self.verified.is_none()
            && self.operations.is_empty()
    }
    pub fn ready(&self) -> bool {
        self.worktree_path().is_some() || (self.verified.is_some() && self.complete())
    }
    pub fn complete(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| {
            self.manifest_offset == m.manifest_bytes && self.bundle_offset == m.bundle_bytes
        })
    }
}
fn transfer_fits(manifest: &TransferManifest, range: u64) -> bool {
    manifest.bundle_bytes > 0
        && manifest.manifest_bytes <= 1_048_576
        && manifest
            .manifest_bytes
            .div_ceil(range)
            .checked_add(manifest.bundle_bytes.div_ceil(range))
            .is_some_and(|n| n <= u64::from(MAX_RANGES))
}
fn transfer(s: &SourceDownload, m: &TransferManifest, attempt: &Attempt) -> Result<()> {
    m.validate()?;
    let b = &attempt.reservation.offer.offered.offer.binding;
    let attempt_ref = &m.attempt;
    let SessionIdentity::Remote { session_id, .. } = b.session else {
        anyhow::bail!("source remote binding")
    };
    ensure!(
        m.role == SourceRole::Base
            && m.transfer_id == m.artifact_id
            && transfer_fits(m, u64::from(s.range_bytes))
            && (
                attempt_ref.attempt_id,
                attempt_ref.session_id,
                attempt_ref.generation
            ) == (b.attempt_id, session_id, b.assignment_generation)
            && m.resolved_spec_digest == b.execution_spec_digest.as_str()
            && matches_source(m, s.admission()),
        "native source manifest association"
    );
    Ok(())
}
fn matches_source(m: &TransferManifest, source: &SourceAdmission) -> bool {
    (m.source.repository_id, m.source.binding_generation)
        == (source.repository_id, source.repository_binding_generation)
        && m.source.base.hex == source.resolved_object.hex
        && m.checkpoint == m.source.base
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
fn valid_cursor(offset: u64, size: u64, range: u64) -> bool {
    offset == size || (offset < size && offset.is_multiple_of(range))
}
fn validate_cursors(s: &SourceDownload, a: &Attempt) -> Result<()> {
    if let Some(m) = &s.manifest {
        transfer(s, m, a)?;
        let range_bytes = u64::from(s.range_bytes);
        ensure!(
            m.manifest_bytes
                .checked_add(m.bundle_bytes)
                .is_some_and(|n| n <= s.reserved_bytes)
                && valid_cursor(s.manifest_offset, m.manifest_bytes, range_bytes)
                && valid_cursor(s.bundle_offset, m.bundle_bytes, range_bytes)
                && (s.bundle_offset == 0 || s.manifest_offset == m.manifest_bytes)
                && u64::from(s.ranges)
                    == s.manifest_offset.div_ceil(range_bytes)
                        + s.bundle_offset.div_ceil(range_bytes)
                        + u64::from(s.pending.is_some()),
            "source cursor bounds"
        );
    } else {
        ensure!(s.undiscovered(), "source before discovery");
    }
    Ok(())
}
fn record_id(ids: &mut BTreeSet<Uuid>, id: Uuid) -> bool {
    !id.is_nil() && ids.insert(id)
}
fn validate_pending(s: &SourceDownload, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    if let Some(r) = &s.pending {
        let m = s.manifest.as_ref().context("range without manifest")?;
        let (offset, size) = match r.file {
            TransferFile::Manifest => (s.manifest_offset, m.manifest_bytes),
            TransferFile::Bundle => (s.bundle_offset, m.bundle_bytes),
        };
        ensure!(
            record_id(ids, r.operation_id)
                && r.offset == offset
                && r.offset < size
                && u64::from(r.length) == (size - r.offset).min(u64::from(s.range_bytes))
                && s.ranges > 0,
            "source pending range"
        );
    }
    Ok(())
}
fn validate_verified(s: &SourceDownload) -> Result<()> {
    if let Some(native) = &s.verified {
        let m = s.manifest.as_ref().context("verified without manifest")?;
        ensure!(
            s.complete()
                && s.pending.is_none()
                && (
                    native.repository_id,
                    &native.repository_identity,
                    native.binding_generation
                ) == (
                    m.source.repository_id,
                    &m.source.logical_identity,
                    m.source.binding_generation
                )
                && native.transfer_id == m.artifact_id
                && (
                    native.format_version,
                    native.object_format.as_str(),
                    native.reference_only
                ) == (1, "sha1", false)
                && (&native.base, &native.source) == (&m.source.base, &m.checkpoint)
                && (native.bundle_size, &native.bundle_sha256)
                    == (m.bundle_bytes, &m.bundle_digest),
            "source verified associations"
        );
    }
    Ok(())
}
pub(super) fn validate(a: &Attempt, revision: u64, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    let Some(s) = &a.source else { return Ok(()) };
    validate_plan(s, a, revision, ids)?;
    validate_cursors(s, a)?;
    validate_pending(s, ids)?;
    validate_verified(s)?;
    validate_operations(s, ids)
}
fn validate_plan(
    s: &SourceDownload,
    a: &Attempt,
    revision: u64,
    ids: &mut BTreeSet<Uuid>,
) -> Result<()> {
    s.preparation.validate(
        a,
        super::inputs::Creation::new(s.created_revision, s.created_ns),
        revision,
    )?;
    s.spec
        .validate_against(&a.reservation.offer.offered.offer)?;
    ensure!(
        s.spec.artifact()? == s.spec_artifact
            && s.root.is_absolute()
            && (1..=a.reservation.disk_bytes).contains(&s.reserved_bytes)
            && record_id(ids, s.discovery_id)
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
    let attempt_id = a.reservation.offer.offered.offer.binding.attempt_id;
    ensure!(
        s.worktree_matches(attempt_id),
        "invalid source worktree plan"
    );
    Ok(())
}
fn validate_operations(s: &SourceDownload, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    for (key, op) in &s.operations {
        ensure!(
            s.verified.is_some()
                && record_id(ids, op.operation_id)
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
        let a = self.attempt(id)?;
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
            worktree: None,
        };
        let mut next = self.next()?;
        next.attempt_mut(id)?.source = Some(plan.clone());
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
    pub(crate) fn source_worktree(&mut self, id: Uuid, path: PathBuf) -> Result<()> {
        self.source_update(id, |s| {
            ensure!(
                s.manifest.is_some()
                    && s.ranges == 0
                    && s.worktree.as_ref().is_none_or(|old| *old == path),
                "source worktree differs from its recorded plan"
            );
            s.worktree = Some(path);
            Ok(())
        })
    }
    pub(crate) fn source_range(&mut self, id: Uuid) -> Result<SourceRange> {
        self.source_update(id, |s| {
            if let Some(old) = &s.pending {
                return Ok(old.clone());
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
            Ok(r)
        })
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
        self.source_update(id, |s| {
            if let Some(old) = s.operations.get(key) {
                return Ok(old.clone());
            }
            ensure!(s.verified.is_some(), "source files not verified");
            let operation_id = Uuid::new_v4();
            let op = PendingGitOperation {
                operation_id,
                request: request(s, key, operation_id)?,
                result: None,
            };
            s.operations.insert(key.into(), op.clone());
            Ok(op)
        })
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
    fn source_update<T>(
        &mut self,
        id: Uuid,
        change: impl FnOnce(&mut SourceDownload) -> Result<T>,
    ) -> Result<T> {
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
        let result = change(s)?;
        if !same(&next.attempts[&id].source, &self.state.attempts[&id].source)? {
            self.persist(next)?;
        }
        Ok(result)
    }
}
