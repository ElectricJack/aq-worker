use super::*;

impl ResultHistory {
    pub(super) fn validate_scope_identity(&self) -> Result<()> {
        let s = &self.scope;
        s.binding.validate()?;
        s.installation.validate()?;
        s.source.validate()?;
        ensure!(
            self.schema_version == 1
                && s.contract == local::artifact()
                && !s.kernel_boot_id.is_nil()
                && s.created_ns < s.original_deadline_ns
                && s.original_deadline_ns <= s.original_hard_deadline_ns
                && s.recorded_revision > 0
                && s.git_pin.inode > 0
                && s.workspace_pin.inode > 0,
            "result scope identity/clocks"
        );
        Ok(())
    }
    pub(super) fn validate_scope_paths(&self) -> Result<()> {
        let s = &self.scope;
        ensure!(
            s.root.is_absolute()
                && s.root.as_os_str().len() <= 2048
                && s.workspace.path.is_absolute()
                && s.managed_root.is_absolute()
                && !s.workspace.workspace_id.is_nil()
                && s.workspace.generation > 0
                && !s.root.starts_with(&s.workspace.path)
                && !s.workspace.path.starts_with(&s.root),
            "result local/source path scope"
        );
        Ok(())
    }
    pub(super) fn validate_reservation(&self) -> Result<()> {
        let s = &self.scope;
        ensure!(
            s.workspace_limit_bytes > 0
                && s.artifact_limit_bytes > 0
                && s.workspace_limit_bytes
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(s.artifact_limit_bytes))
                    .is_some_and(|n| n <= s.reserved_disk_bytes),
            "result original source reservation"
        );
        Ok(())
    }
    pub(super) fn validate_scope_operations(&self) -> Result<()> {
        let s = &self.scope;
        let mut ids = BTreeSet::new();
        for id in [s.binding.operation_id, s.binding.report_id, s.binding.nonce]
            .into_iter()
            .chain(s.operations.all())
        {
            ensure!(
                !id.is_nil() && ids.insert(id),
                "result operation identity reused"
            );
        }
        if let StorageStage::Ready {
            pins,
            recorded_revision,
        } = &s.storage
        {
            ensure!(
                pins.root.inode > 0
                    && pins.submissions.inode > 0
                    && *recorded_revision > s.recorded_revision,
                "result directory pins absent"
            );
        }
        Ok(())
    }
}
