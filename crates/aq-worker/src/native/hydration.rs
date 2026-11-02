//! Native-only source collection and actual Git ownership. No Prepare gate.
use super::*;
use crate::source::{GitClient, Workspace};
use aq_worker_protocol::TransferManifest;
use serde_json::{Value, json};
#[path = "hydration/files.rs"]
mod files;
/// Fresh local owner result, intentionally neither serializable nor constructible
/// outside this owner. It is not launch authority and cannot be reopened from a
/// journal receipt. Later Prepare must join it with all original inputs.
pub struct HydratedWorkspace {
    workspace: Workspace,
    manifest: TransferManifest,
    attempt: Uuid,
    binding: aq_session_protocol::LaunchBinding,
    observed_ns: u64,
    deadline_ns: u64,
    _stage_owner: Arc<files::Staging>,
    git: GitClient,
}
impl HydratedWorkspace {
    pub(super) fn revalidate(&self, journal: &Journal) -> Result<()> {
        self.revalidate_until(journal, self.deadline_ns)
    }
    // Only the durable execution owner supplies this independent authority cap.
    // The original preparation proof deadline remains unchanged. Current local
    // Git retention/workspace inspection is required again; no export is renewed.
    pub(super) fn revalidate_execution(&self, journal: &Journal, deadline: u64) -> Result<()> {
        self.revalidate_until(journal, deadline)
    }
    fn revalidate_until(&self, journal: &Journal, deadline: u64) -> Result<()> {
        ensure!(boot_ns()? < deadline, "native source proof expired");
        let a = journal
            .state()
            .attempts
            .get(&self.attempt)
            .context("source attempt absent")?;
        let s = a.source.as_ref().context("source owner absent")?;
        ensure!(
            self.binding == a.reservation.offer.offered.offer.binding
                && s.manifest.as_ref() == Some(&self.manifest),
            "source join association"
        );
        self._stage_owner.verify(&self.manifest)?;
        let retention = self.git.exchange(
            &s.operations
                .get("retention")
                .context("retention intent absent")?
                .request,
        )?;
        let inspect = self.git.exchange(
            &s.operations
                .get("workspace")
                .context("workspace intent absent")?
                .request,
        )?;
        let imported = s
            .operations
            .get("import")
            .and_then(|o| o.result.as_ref())
            .context("import receipt absent")?;
        let created = s
            .operations
            .get("create")
            .and_then(|o| o.result.as_ref())
            .context("workspace receipt absent")?;
        ensure!(
            serde_json::to_value(&self.workspace)?
                == serde_json::to_value(serde_json::from_value::<Workspace>(created.clone())?)?,
            "workspace owner changed"
        );
        validate_current(&self.git, s, imported, created, &retention, &inspect)?;
        ensure!(boot_ns()? < deadline, "source join expired");
        Ok(())
    }
    pub(super) fn git(&self) -> &GitClient {
        &self.git
    }
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub fn attempt_id(&self) -> Uuid {
        self.attempt
    }
    pub fn binding(&self) -> &aq_session_protocol::LaunchBinding {
        &self.binding
    }
    pub fn observed_ns(&self) -> u64 {
        self.observed_ns
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn manifest(&self) -> &TransferManifest {
        &self.manifest
    }
}
impl InputPipeline {
    pub async fn hydrate_source(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
        spec: VerifiedArtifact,
        git: GitClient,
    ) -> Result<HydratedWorkspace> {
        self.hydrate_source_context(id, InputContext::Legacy(preparation), spec, git)
            .await
    }
    pub async fn hydrate_source_phase(
        &self,
        id: Uuid,
        preparation: Arc<crate::native::http::preparation_v2::AcceptedPreparation>,
        spec: VerifiedArtifact,
        git: GitClient,
    ) -> Result<HydratedWorkspace> {
        self.hydrate_source_context(id, InputContext::Phase(preparation), spec, git)
            .await
    }
    pub async fn hydrate_source_refresh(
        &self,
        id: Uuid,
        preparation: Arc<crate::native::http::preparation_refresh_v1::AcceptedPreparation>,
        spec: VerifiedArtifact,
        git: GitClient,
    ) -> Result<HydratedWorkspace> {
        self.hydrate_source_context(id, InputContext::Refresh(preparation), spec, git)
            .await
    }
    async fn hydrate_source_context(
        &self,
        id: Uuid,
        preparation: InputContext,
        spec: VerifiedArtifact,
        git: GitClient,
    ) -> Result<HydratedWorkspace> {
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native source bulk lane overloaded")?;
        let observed = preparation.clone();
        let local = git.clone();
        let plan = self
            .storage(move |owner| {
                observed.check(&owner.journal, id)?;
                owner.journal.begin_source(id, &spec, &local)
            })
            .await?;
        let binding = preparation.binding().clone();
        let manifest = self
            .client
            .source_status(&binding, plan.admission(), plan.manifest.as_ref())
            .await?;
        let m = manifest.clone();
        let observed = preparation.clone();
        self.storage(move |owner| {
            observed.check(&owner.journal, id)?;
            owner.journal.source_manifest(id, m)
        })
        .await?;
        let root = git.root().to_owned();
        let marker = serde_json::to_vec(
            &json!({"schema_version":1,"attempt":id,"spec":plan.spec_artifact,"manifest":manifest}),
        )?;
        let m = manifest.clone();
        let observed = preparation.clone();
        let stage = self
            .storage(move |owner| {
                observed.check(&owner.journal, id)?;
                files::Staging::open(&root, &m, &marker)
            })
            .await?;
        let stage = Arc::new(stage);
        loop {
            let state = self.snapshot().await?;
            let source = state.attempts[&id]
                .source
                .as_ref()
                .context("source plan missing")?;
            if source.complete() {
                break;
            }
            let observed = preparation.clone();
            let range = self
                .storage(move |owner| {
                    observed.check(&owner.journal, id)?;
                    owner.journal.source_range(id)
                })
                .await?;
            ensure!(
                boot_ns()?
                    < plan
                        .preparation
                        .deadline_ns()
                        .min(preparation.deadline_ns()),
                "native source preparation expired"
            );
            let chunk = self
                .client
                .source_chunk(
                    &binding,
                    &manifest,
                    range.file.clone(),
                    range.offset,
                    range.length,
                )
                .await?;
            let disk = stage.clone();
            let observed = preparation.clone();
            self.storage(move |owner| {
                observed.check(&owner.journal, id)?;
                disk.append(&range.file, range.offset, &chunk.bytes)?;
                owner.journal.source_range_synced(id, &range)
            })
            .await?;
        }
        let disk = stage.clone();
        let m = manifest.clone();
        let observed = preparation.clone();
        self.storage(move |owner| {
            observed.check(&owner.journal, id)?;
            let verified = disk.verify(&m)?;
            owner.journal.source_verified(id, verified)?;
            disk.publish(&m)
        })
        .await?;
        // Every exchange runs behind the bounded storage lane, after the exact
        // request is durable. Cancellation retains that lane and its real owner.
        for key in ["repository", "import", "create"] {
            self.native_git(id, preparation.clone(), git.clone(), key)
                .await?;
        }
        let retention = self
            .native_git(id, preparation.clone(), git.clone(), "retention")
            .await?;
        let inspect = self
            .native_git(id, preparation.clone(), git.clone(), "workspace")
            .await?;
        let disk = stage.clone();
        let m = manifest.clone();
        self.storage(move |owner| {
            preparation.check(&owner.journal, id)?;
            disk.verify(&m)?;
            let s = owner.journal.state().attempts[&id]
                .source
                .as_ref()
                .context("source plan absent")?;
            let imported = s.operations["import"]
                .result
                .as_ref()
                .context("import receipt absent")?;
            let created = s.operations["create"]
                .result
                .as_ref()
                .context("create receipt absent")?;
            validate_current(&git, s, imported, created, &retention, &inspect)?;
            preparation.check(&owner.journal, id)?;
            let workspace: Workspace = serde_json::from_value(created.clone())?;
            let observed_ns = boot_ns()?;
            let deadline_ns = preparation.deadline_ns().min(s.preparation.deadline_ns());
            ensure!(observed_ns < deadline_ns, "native source proof expired");
            Ok(HydratedWorkspace {
                workspace,
                manifest: m,
                attempt: id,
                binding: preparation.binding().clone(),
                observed_ns,
                deadline_ns,
                _stage_owner: disk,
                git,
            })
        })
        .await
    }
    async fn native_git(
        &self,
        id: Uuid,
        preparation: InputContext,
        git: GitClient,
        key: &'static str,
    ) -> Result<Value> {
        self.storage(move |owner| {
            preparation.check(&owner.journal, id)?;
            let op = owner.journal.source_operation(id, key)?;
            // Cached mutation replies are exact historical receipts. Read calls
            // always exchange again; saved JSON cannot become a current proof.
            let result = if matches!(key, "import" | "create") {
                if let Some(old) = op.result {
                    old
                } else {
                    git.exchange(&op.request)?
                }
            } else {
                git.exchange(&op.request)?
            };
            preparation.check(&owner.journal, id)?;
            let source = owner.journal.state().attempts[&id]
                .source
                .as_ref()
                .context("source plan absent")?;
            if key == "repository" {
                let b = source.binding()?;
                ensure!(
                    result["repository_id"] == b.repository_id.to_string()
                        && result["binding_generation"] == b.binding_generation
                        && result["identity"] == b.logical_identity
                        && result["mode"] == "worker",
                    "native Git repository binding"
                );
            }
            owner.journal.source_result(id, key, result.clone())?;
            Ok(result)
        })
        .await
    }
}
fn validate_current(
    git: &GitClient,
    s: &crate::native::journal::SourceDownload,
    imported: &Value,
    created: &Value,
    retention: &Value,
    inspect: &Value,
) -> Result<()> {
    let b = s.binding()?;
    let m = s.manifest.as_ref().context("source manifest absent")?;
    let native = s.verified.as_ref().context("verified manifest absent")?;
    let handle = imported["retention_handle"]
        .as_str()
        .and_then(|v| Uuid::parse_str(v).ok())
        .context("import retention absent")?;
    ensure!(
        !handle.is_nil()
            && imported["repository_id"] == b.repository_id.to_string()
            && imported["binding_generation"] == b.binding_generation
            && imported["transfer_id"] == m.artifact_id.to_string()
            && imported["source"] == serde_json::to_value(&b.base)?
            && imported["manifest_digest"] == native.canonical_digest()?,
        "native import receipt pins"
    );
    // handle_id inspection has a direct handle result (list inspection has a
    // separate handles envelope). Never accept an arbitrary matching list item.
    ensure!(
        retention["id"] == handle.to_string()
            && retention["repository_id"] == b.repository_id.to_string()
            && retention["binding_generation"] == b.binding_generation
            && retention["state"] == "active"
            && retention["generation"].as_u64().is_some_and(|v| v > 0)
            && retention["refs"]
                .as_array()
                .is_some_and(|refs| refs.iter().any(|r| r["oid"] == b.base.hex)),
        "native import is not currently retained"
    );
    let workspace: Workspace = serde_json::from_value(created.clone())?;
    git.validate_workspace(&workspace)?;
    ensure!(
        inspect["workspace"] == *created
            && created["repository_id"] == b.repository_id.to_string()
            && created["binding_generation"] == b.binding_generation
            && created["base"] == b.base.hex
            && created["lifecycle"] == "active"
            && created["allocating_operation"] == s.operations["create"].operation_id.to_string()
            && inspect["head"] == serde_json::to_value(&b.base)?
            && inspect["clean"] == true,
        "native workspace is not current exact clean base"
    );
    Ok(())
}
