//! Native-only source collection and actual Git ownership. No Prepare gate.
use super::*;
use crate::native::journal::SourceDownload;
use crate::source::{GitClient, Workspace};
use aq_worker_protocol::TransferManifest;
#[path = "hydration/download.rs"]
mod download;
#[path = "hydration/receipts.rs"]
mod receipts;
use receipts::SourceReceipts;
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
    /// None for a worktree of the node's repository.
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
            aq_node_git::workspaces::load(&self.workspace.path)?.has_resources()
                == a.reservation.workspace_required,
            "workspace_required_mismatch: frozen supplemental checkout absent or changed",
        );
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
        SourceReceipts {
            imported,
            created,
            retention: &retention,
            inspect: &inspect,
        }
        .verify(&self.git, s)?;
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
        InputAttempt { id, preparation }: InputAttempt,
        spec: VerifiedArtifact,
        git: GitClient,
    ) -> Result<HydratedWorkspace> {
        let attempt = SourceHydration {
            pipeline: self,
            id,
            preparation,
            git,
        };
        attempt.collect(spec).await
    }
}

struct SourceHydration<'a> {
    pipeline: &'a InputPipeline,
    id: Uuid,
    preparation: InputContext,
    git: GitClient,
}
impl SourceHydration<'_> {
    async fn storage<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Journal) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let id = self.id;
        let observed = self.preparation.clone();
        self.pipeline
            .storage(move |owner| {
                observed.check(&owner.journal, id)?;
                action(&mut owner.journal)
            })
            .await
    }
    async fn collect(self, spec: VerifiedArtifact) -> Result<HydratedWorkspace> {
        let _bulk = self
            .pipeline
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native source bulk lane overloaded")?;
        let id = self.id;
        let local = self.git.clone();
        let plan = self
            .storage(move |journal| journal.begin_source(id, &spec, &local))
            .await?;
        let deadline = plan
            .preparation
            .deadline_ns()
            .min(self.preparation.deadline_ns());
        let workspace_required = self
            .storage(move |journal| {
                Ok(journal.state().attempts[&id].reservation.workspace_required)
            })
            .await?;
        ensure!(
            !workspace_required || self.git.node.is_some(),
            "workspace_required: supplemental task resources need node Git configured",
        );
        let grant = self.grant(deadline).await?;
        let client = self.pipeline.client.fetch_until(deadline);
        let (manifest, maximum_range) = client
            .source_status_with_range(
                self.preparation.binding(),
                plan.admission(),
                plan.manifest.as_ref(),
            )
            .await?;
        let copy = manifest.clone();
        self.storage(move |journal| journal.source_manifest(id, copy, maximum_range))
            .await?;
        if let (Some(node), Some(grant)) = (&self.git.node, grant) {
            let workspace = self.checkout((node, grant), &plan, &manifest).await?;
            start_siblings(node, id, &workspace.path, deadline, workspace_required).await?;
            let observed_ns = boot_ns()?;
            ensure!(observed_ns < deadline, "native source proof expired");
            return Ok(HydratedWorkspace {
                workspace,
                manifest,
                attempt: id,
                binding: self.preparation.binding().clone(),
                observed_ns,
                deadline_ns: deadline,
                _stage_owner: None,
                git: self.git,
            });
        }
        ensure!(
            plan.worktree_path().is_none(),
            "recorded node worktree requires a git grant"
        );
        let stage = self.open_stage(&plan, &manifest).await?;
        self.download((&client, deadline), &manifest, &stage)
            .await?;
        let hydrated = self.complete_bundle(manifest, stage).await?;
        if let Some(node) = &hydrated.git.node {
            start_siblings(
                node,
                id,
                &hydrated.workspace.path,
                deadline,
                workspace_required,
            )
            .await?;
        }
        Ok(hydrated)
    }
    async fn grant(&self, deadline: u64) -> Result<Option<aq_node_git::Grant>> {
        let Some(node) = &self.git.node else {
            return Ok(None);
        };
        let remaining = deadline
            .checked_sub(boot_ns()?)
            .context("source preparation expired")?;
        node.grant(self.id, std::time::Duration::from_nanos(remaining))
            .await
    }
    async fn checkout(
        &self,
        checkout: (&aq_node_git::Node, aq_node_git::Grant),
        plan: &SourceDownload,
        manifest: &TransferManifest,
    ) -> Result<Workspace> {
        let (node, grant) = checkout;
        let id = self.id;
        ensure!(
            plan.ranges == 0
                && grant.repository_id == manifest.source.repository_id
                && grant.base == manifest.source.base.hex
                && grant.task_ref == format!("refs/heads/aq/{}", plan.spec.task.task_id),
            "git grant differs from source plan"
        );
        let path = self.git.root().join("worktrees").join(id.to_string());
        let author = self
            .pipeline
            .storage(move |owner| {
                Ok(owner.journal.state().attempts[&id]
                    .reservation
                    .git_author
                    .clone())
            })
            .await?;
        node.start_as(id, path.clone(), grant, author).await?;
        let worktree = path.clone();
        self.storage(move |journal| journal.source_worktree(id, worktree))
            .await?;
        Ok(Workspace {
            workspace_id: id,
            generation: 1,
            path,
        })
    }
}

async fn start_siblings(
    node: &aq_node_git::Node,
    attempt: Uuid,
    workspace: &std::path::Path,
    deadline: u64,
    required: bool,
) -> Result<()> {
    let remaining = deadline
        .checked_sub(boot_ns()?)
        .context("sibling workspace preparation expired")?;
    node.start_siblings(
        attempt,
        workspace,
        std::time::Duration::from_nanos(remaining),
        required,
    )
    .await?;
    ensure!(
        boot_ns()? < deadline,
        "sibling workspace preparation expired"
    );
    Ok(())
}
