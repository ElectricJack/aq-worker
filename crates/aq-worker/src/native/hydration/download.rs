//! Resume the source bundle through its durable file cursor and retained Git operations.
use super::*;
impl SourceHydration<'_> {
    pub(super) async fn open_stage(
        &self,
        plan: &SourceDownload,
        manifest: &TransferManifest,
    ) -> Result<Arc<files::Staging>> {
        let id = self.id;
        let root = self.git.root().to_owned();
        let marker = serde_json::to_vec(
            &json!({"schema_version":1,"attempt":id,"spec":plan.spec_artifact,"manifest":manifest}),
        )?;
        let manifest = manifest.clone();
        self.storage(move |_| files::Staging::open(&root, &manifest, &marker).map(Arc::new))
            .await
    }
    pub(super) async fn download(
        &self,
        source: (&NativeClient, u64),
        manifest: &TransferManifest,
        stage: &Arc<files::Staging>,
    ) -> Result<()> {
        let (client, deadline) = source;
        let id = self.id;
        loop {
            let state = self.pipeline.snapshot().await?;
            let source = state.attempts[&id]
                .source
                .as_ref()
                .context("source plan missing")?;
            if source.complete() {
                break;
            }
            let range = self
                .storage(move |journal| journal.source_range(id))
                .await?;
            ensure!(boot_ns()? < deadline, "native source preparation expired");
            let chunk = client
                .source_chunk(
                    self.preparation.binding(),
                    manifest,
                    range.file.clone(),
                    range.offset,
                    range.length,
                )
                .await?;
            let disk = stage.clone();
            self.storage(move |journal| {
                disk.append(&range.file, range.offset, &chunk.bytes)?;
                journal.source_range_synced(id, &range)
            })
            .await?;
        }
        let disk = stage.clone();
        let manifest = manifest.clone();
        self.storage(move |journal| {
            let verified = disk.verify(&manifest)?;
            journal.source_verified(id, verified)?;
            disk.publish(&manifest)
        })
        .await
    }
    pub(super) async fn complete_bundle(
        self,
        manifest: TransferManifest,
        stage: Arc<files::Staging>,
    ) -> Result<HydratedWorkspace> {
        for key in ["repository", "import", "create"] {
            self.native_git(key).await?;
        }
        let retention = self.native_git("retention").await?;
        let inspect = self.native_git("workspace").await?;
        let Self {
            pipeline,
            id,
            preparation,
            git,
        } = self;
        pipeline
            .storage(move |owner| {
                preparation.check(&owner.journal, id)?;
                stage.verify(&manifest)?;
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
                SourceReceipts {
                    imported,
                    created,
                    retention: &retention,
                    inspect: &inspect,
                }
                .verify(&git, s)?;
                preparation.check(&owner.journal, id)?;
                let workspace: Workspace = serde_json::from_value(created.clone())?;
                let observed_ns = boot_ns()?;
                let deadline_ns = preparation.deadline_ns().min(s.preparation.deadline_ns());
                ensure!(observed_ns < deadline_ns, "native source proof expired");
                Ok(HydratedWorkspace {
                    workspace,
                    manifest,
                    attempt: id,
                    binding: preparation.binding().clone(),
                    observed_ns,
                    deadline_ns,
                    _stage_owner: Some(stage),
                    git,
                })
            })
            .await
    }
    async fn native_git(&self, key: &'static str) -> Result<Value> {
        let id = self.id;
        let preparation = self.preparation.clone();
        let git = self.git.clone();
        self.pipeline
            .storage(move |owner| {
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
