use super::*;

impl ResultHistory {
    pub(super) fn validate_source(&self) -> Result<()> {
        let h = &self.source;
        self.validate_source_operations()?;
        self.validate_checkpoint()?;
        if let Some(e) = &h.export {
            self.validate_export(e)?;
        }
        if let Some(t) = &h.transfer {
            self.validate_transfer(t)?;
        }
        self.validate_source_progress()?;
        Ok(())
    }
    fn validate_source_operations(&self) -> Result<()> {
        let h = &self.source;
        let completed = |key: &str| {
            h.operations
                .get(key)
                .is_some_and(|operation| operation.result.is_some())
        };
        ensure!(
            h.operations.len() <= 2
                && h.checkpoint.is_some() == completed("checkpoint")
                && h.export.is_some() == completed("export")
                && h.transfer.is_some() == h.upload.is_some(),
            "result source history projection"
        );
        for (key, op) in &h.operations {
            self.validate_source_operation(key, op)?;
        }
        Ok(())
    }
    fn validate_source_operation(&self, key: &str, op: &PendingGitOperation) -> Result<()> {
        let h = &self.source;
        let s = &self.scope;
        ensure!(
            self.claim.is_some() && self.stop.is_some(),
            "result source action before claim/actual stop"
        );
        let (id, action) = match key {
            "checkpoint" => (
                s.operations.checkpoint,
                crate::source::GitAction::Checkpoint {
                    workspace_id: s.workspace.workspace_id,
                    generation: s.workspace.generation,
                    head: h.head.clone().context("result checkpoint head absent")?,
                },
            ),
            "export" => {
                let c = h
                    .checkpoint
                    .as_ref()
                    .context("result export without checkpoint")?;
                (
                    s.operations.export,
                    crate::source::GitAction::Export {
                        source: c.source.clone(),
                        retention_handle: c.retention_handle,
                    },
                )
            }
            _ => anyhow::bail!("unknown result source operation"),
        };
        let request = action.request_as(&s.source, id, s.git_author.as_ref());
        // An export intent journaled before exports named their base as a
        // prerequisite stays valid, so a result already finishing when the
        // worker is replaced keeps its original operation.
        let mut whole = request.clone();
        if let Some(params) = whole["params"].as_object_mut() {
            params.remove("confirmed_prerequisites");
        }
        ensure!(
            op.operation_id == id
                && (op.request == request || (key == "export" && op.request == whole)),
            "result source original request changed"
        );
        if let Some(value) = &op.result {
            ensure!(
                serde_json::to_vec(value)?.len() <= MAX_GIT_REPLY_BYTES,
                "result source reply bound"
            );
        }
        Ok(())
    }
    fn validate_checkpoint(&self) -> Result<()> {
        let h = &self.source;
        let s = &self.scope;
        if let Some(head) = &h.head {
            head.validate()?;
        }
        if let Some(bytes) = h.workspace_bytes {
            ensure!(
                bytes <= s.workspace_limit_bytes,
                "result actual workspace budget"
            );
        }
        if let Some(c) = &h.checkpoint {
            c.source.validate()?;
            ensure!(
                c.workspace_id == s.workspace.workspace_id
                    && c.workspace_generation >= s.workspace.generation
                    && !c.retention_handle.is_nil()
                    && serde_json::to_value(c)?
                        == serde_json::to_value(serde_json::from_value::<Checkpoint>(
                            h.operations["checkpoint"].result.clone().unwrap()
                        )?)?,
                "result checkpoint original owner/result"
            );
        }
        Ok(())
    }
    fn validate_source_progress(&self) -> Result<()> {
        let h = &self.source;
        let s = &self.scope;
        if let Some(u) = &h.upload {
            let t = h
                .transfer
                .as_ref()
                .context("result upload before transport")?;
            let aq_session_protocol::SessionIdentity::Remote {
                node_id, boot_id, ..
            } = s.binding.launch.session
            else {
                anyhow::bail!("result worker identity")
            };
            u.validate(t, node_id)?;
            ensure!(
                u.identity.boot_id == boot_id
                    && (u.begin_operation, u.finalize_operation)
                        == (s.operations.upload_begin, s.operations.upload_finalize),
                "result upload original identity"
            );
        }
        if let Some(r) = &h.receipt {
            let t = h
                .transfer
                .as_ref()
                .context("result receipt before transport")?;
            let u = h.upload.as_ref().unwrap();
            ensure!(
                u.begun
                    && u.pending.is_none()
                    && (u.manifest_offset, u.bundle_offset) == (t.manifest_bytes, t.bundle_bytes),
                "result source receipt before complete upload"
            );
            let e = h.export.as_ref().unwrap();
            crate::source_transfer::steps::validate_retention_claim(
                r,
                t,
                &e.manifest,
                (&s.source, &e.manifest.source),
            )?;
        }
        Ok(())
    }
}

mod artifact;
