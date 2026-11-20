//! Native-only source collection and actual Git ownership. No Prepare gate.
use super::*;
use crate::source::{GitClient, Workspace};
use aq_worker_protocol::TransferManifest;
use serde_json::{Value, json};
#[path = "hydration/files.rs"]
pub(crate) mod files;
/// Fresh local source proof. Prepare must join it with the original inputs.
pub struct HydratedWorkspace {
    workspace: Workspace,
    manifest: TransferManifest,
    attempt: Uuid,
    binding: aq_session_protocol::LaunchBinding,
    observed_ns: u64,
    deadline_ns: u64,
    /// None for a worktree of the node's repository (`delivery_playbook`).
    _stage_owner: Option<Arc<files::Staging>>,
    git: GitClient,
}
impl HydratedWorkspace {
    pub(super) fn revalidate(&self, journal: &Journal) -> Result<()> {
        self.revalidate_until(journal, self.deadline_ns)
    }
    // The durable execution owner supplies its own deadline and rechecks local Git.
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
        let Some(stage) = &self._stage_owner else {
            ensure!(
                s.worktree_path() == Some(&self.workspace.path) && self.workspace.path.is_dir(),
                "source worktree absent"
            );
            return Ok(());
        };
        stage.verify(&self.manifest)?;
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
    input_entries! { hydrate_source_context(spec: VerifiedArtifact, git: GitClient) -> HydratedWorkspace {
        hydrate_source: Legacy(Preparation),
        hydrate_source_phase: Phase(super::Phase),
        hydrate_source_refresh: Refresh(super::Refresh)
    } }
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
        let deadline = plan
            .preparation
            .deadline_ns()
            .min(preparation.deadline_ns());
        #[cfg(feature = "delivery_playbook")]
        let grant = if let Some(node) = &git.node {
            let remaining = deadline
                .checked_sub(boot_ns()?)
                .context("source preparation expired")?;
            node.grant(id, std::time::Duration::from_nanos(remaining))
                .await?
        } else {
            None
        };
        let client = self.client.fetch_until(deadline);
        let (manifest, maximum_range) =
            (client.source_status_with_range(&binding, plan.admission(), plan.manifest.as_ref()))
                .await?;
        let m = manifest.clone();
        let observed = preparation.clone();
        self.storage(move |owner| {
            observed.check(&owner.journal, id)?;
            owner.journal.source_manifest(id, m, maximum_range)
        })
        .await?;
        #[cfg(feature = "delivery_playbook")]
        if let (Some(node), Some(grant)) = (&git.node, grant) {
            ensure!(
                plan.ranges == 0
                    && grant.repository_id == manifest.source.repository_id
                    && grant.base == manifest.source.base.hex
                    && grant.task_ref == format!("refs/heads/aq/{}", plan.spec.task.task_id),
                "git grant differs from source plan"
            );
            let path = git.root().join("worktrees").join(id.to_string());
            let author = self
                .storage(move |owner| {
                    Ok(owner.journal.state().attempts[&id]
                        .reservation
                        .git_author
                        .clone())
                })
                .await?;
            node.start_as(id, path.clone(), grant, author).await?;
            let (observed, worktree) = (preparation.clone(), path.clone());
            self.storage(move |owner| {
                observed.check(&owner.journal, id)?;
                owner.journal.source_worktree(id, worktree)
            })
            .await?;
            let observed_ns = boot_ns()?;
            ensure!(observed_ns < deadline, "native source proof expired");
            return Ok(HydratedWorkspace {
                workspace: Workspace {
                    workspace_id: id,
                    generation: 1,
                    path,
                },
                manifest,
                attempt: id,
                binding,
                observed_ns,
                deadline_ns: deadline,
                _stage_owner: None,
                git,
            });
        }
        ensure!(
            plan.worktree_path().is_none(),
            "recorded node worktree requires a git grant"
        );
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
            ensure!(boot_ns()? < deadline, "native source preparation expired");
            let chunk = client
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
                _stage_owner: Some(disk),
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
            let source = owner.journal.state().attempts[&id]
                .source
                .as_ref()
                .context("source plan absent")?;
            // Cached mutation replies are exact historical receipts. Read calls
            // always exchange again; saved JSON cannot become a current proof.
            let fresh = || git.exchange(&op.request);
            let result = match op.result.filter(|_| matches!(key, "import" | "create")) {
                Some(old) => old,
                None if key == "repository" => git.registering(source.binding()?, fresh)?,
                None => fresh()?,
            };
            preparation.check(&owner.journal, id)?;
            if key == "repository" {
                GitClient::same_binding(source.binding()?, &result)?;
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
    validate_retained(
        git,
        s.binding()?,
        s.manifest.as_ref().context("source manifest absent")?,
        s.verified.as_ref().context("verified manifest absent")?,
        s.operations["create"].operation_id,
        imported,
        created,
        retention,
        inspect,
    )
}
/// Shared by node and runner hydration: the import, retention handle and
/// workspace must all be the exact verified retained base, currently clean.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_retained(
    git: &GitClient,
    b: &aq_worker_protocol::SourceBinding,
    m: &TransferManifest,
    native: &crate::source::GitManifest,
    create_operation: Uuid,
    imported: &Value,
    created: &Value,
    retention: &Value,
    inspect: &Value,
) -> Result<()> {
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
            && created["allocating_operation"] == create_operation.to_string()
            && inspect["head"] == serde_json::to_value(&b.base)?
            && inspect["clean"] == true,
        "native workspace is not current exact clean base"
    );
    Ok(())
}
