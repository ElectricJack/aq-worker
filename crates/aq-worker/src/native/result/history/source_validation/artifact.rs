use super::*;

impl ResultHistory {
    pub(super) fn validate_export(&self, e: &ExportRecord) -> Result<()> {
        let s = &self.scope;
        let h = &self.source;
        let c = h
            .checkpoint
            .as_ref()
            .context("result export checkpoint absent")?;
        let raw = h.operations["export"].result.as_ref().unwrap();
        ensure!(
            e.valid_profile()
                && e.manifest.prerequisites.iter().all(|p| p == &s.source.base)
                && e.artifact == e.manifest.transfer_id
                && crate::source_transfer::matches_source_binding(&e.manifest, &s.source)
                && e.manifest.source == c.source
                && e.within_bound(s.artifact_limit_bytes)
                && raw["artifact_handle"] == e.artifact.to_string()
                && raw["manifest"] == serde_json::to_value(&e.manifest)?,
            "result exported artifact scope/bytes"
        );
        ensure!(
            h.workspace_bytes
                .context("result actual workspace size absent")?
                .checked_add(e.total_bytes)
                .is_some_and(|n| n <= s.reserved_disk_bytes),
            "result actual retained disk bound"
        );
        Ok(())
    }
    pub(super) fn validate_transfer(&self, t: &TransferManifest) -> Result<()> {
        let s = &self.scope;
        let h = &self.source;
        t.validate()?;
        let e = h.export.as_ref().context("result transfer before export")?;
        let b = &s.binding.launch;
        ensure!(
            (t.transfer_id, t.artifact_id) == (s.operations.transfer, e.artifact)
                && t.role == aq_worker_protocol::SourceRole::Checkpoint
                && t.attempt.attempt_id == b.attempt_id
                && t.attempt.session_id
                    == match b.session {
                        aq_session_protocol::SessionIdentity::Remote { session_id, .. } =>
                            session_id,
                        _ => anyhow::bail!("result requires actual remote session"),
                    }
                && (t.attempt.generation, t.resolved_spec_digest.as_str())
                    == (b.assignment_generation, b.execution_spec_digest.as_str())
                && t.source == s.source
                && e.matches_transfer(t),
            "result transport exact source"
        );
        Ok(())
    }
}

impl ExportRecord {
    fn valid_profile(&self) -> bool {
        !self.artifact.is_nil()
            && self.manifest.format_version == 1
            && self.manifest.object_format == "sha1"
            && !self.manifest.producer_installation_id.is_nil()
            && self.manifest.reference_only == (self.manifest.bundle_size == 0)
            && (self.manifest.bundle_size == 0) == self.manifest.bundle_sha256.is_none()
            && self
                .manifest
                .bundle_sha256
                .as_deref()
                .is_none_or(valid_digest)
    }
    fn within_bound(&self, limit: u64) -> bool {
        (1..=1_048_576).contains(&self.manifest_bytes)
            && self.manifest_bytes.checked_add(self.manifest.bundle_size) == Some(self.total_bytes)
            && self.total_bytes <= limit
            && valid_digest(&self.manifest_digest)
    }
    fn matches_transfer(&self, t: &TransferManifest) -> bool {
        t.checkpoint == self.manifest.source
            && (t.manifest_bytes, &t.manifest_digest)
                == (self.manifest_bytes, &self.manifest_digest)
            && (t.bundle_bytes, &t.bundle_digest)
                == (self.manifest.bundle_size, &self.manifest.bundle_sha256)
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
