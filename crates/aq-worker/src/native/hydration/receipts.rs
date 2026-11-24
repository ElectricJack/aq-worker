//! Compare Git's current retention and worktree state with the journaled operations.
use super::*;
use aq_worker_protocol::SourceBinding;
pub(super) struct SourceReceipts<'a> {
    pub(super) imported: &'a Value,
    pub(super) created: &'a Value,
    pub(super) retention: &'a Value,
    pub(super) inspect: &'a Value,
}
impl SourceReceipts<'_> {
    pub(super) fn verify(&self, git: &GitClient, source: &SourceDownload) -> Result<()> {
        let binding = source.binding()?;
        let manifest = source.manifest.as_ref().context("source manifest absent")?;
        let native = source
            .verified
            .as_ref()
            .context("verified manifest absent")?;
        let operation = source.operations["create"].operation_id;
        let handle = self.import_handle(binding, manifest, native)?;
        self.check_retention(binding, handle)?;
        self.check_workspace(git, binding, operation)
    }
    fn import_handle(
        &self,
        b: &SourceBinding,
        m: &TransferManifest,
        native: &crate::source::GitManifest,
    ) -> Result<Uuid> {
        let imported = self.imported;
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
        Ok(handle)
    }
    fn check_retention(&self, b: &SourceBinding, handle: Uuid) -> Result<()> {
        let retention = self.retention;
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
        Ok(())
    }
    fn check_workspace(
        &self,
        git: &GitClient,
        b: &SourceBinding,
        create_operation: Uuid,
    ) -> Result<()> {
        let created = self.created;
        let inspect = self.inspect;
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
}
