//! Stopped-only, finite source effects through the original Git owner.
use super::*;
use crate::{
    source::{GitAction, PendingGitOperation},
    transfer_client::TransferClient,
};
use aq_worker_protocol::Validate as _;
impl InputPipeline {
    /// Only an actual stopped owner can checkpoint and push. A fresh current
    /// grant is intentionally unnecessary for preserving already-produced
    /// source. The checkpoint waits for the pipeline's one bulk lane instead of
    /// failing a finished task; the push and its admission take no lane.
    pub async fn deliver_result_source(
        &self,
        stopped: &DurableStoppedResult,
        client: &TransferClient,
    ) -> Result<()> {
        let owner = Live(self, stopped);
        {
            let _lane = self.bulk_slots.clone().acquire_owned().await?;
            checkpoint(&owner).await?;
        }
        deliver(&owner, client).await
    }
}
pub(super) async fn snapshot<O: StoppedOwner>(o: &O) -> Result<ResultHistory> {
    o.verify()?;
    let id = o.attempt_id();
    o.journal(move |journal| {
        Ok(journal.state().attempts[&id]
            .result
            .as_ref()
            .context("result history absent")?
            .clone())
    })
    .await
}
/// The finite delivery of a checkpoint: one push, then its admission.
pub(super) async fn deliver<O: StoppedOwner>(o: &O, client: &TransferClient) -> Result<()> {
    for _ in 0..2 {
        if push_step(o, client).await? {
            return Ok(());
        }
    }
    anyhow::bail!("result push step bound exhausted")
}
pub(super) async fn checkpoint<O: StoppedOwner>(o: &O) -> Result<()> {
    const KEY: &str = "checkpoint";
    o.verify()?;
    let id = o.attempt_id();
    let expected = snapshot(o).await?;
    if expected.source.checkpoint.is_some() {
        return Ok(());
    }
    let git = o.git();
    let prepared = if expected.source.operations.contains_key(KEY) {
        expected
    } else {
        let observed = expected.clone();
        // Query/census before freezing ONLY the first checkpoint intent.
        // A retry always uses the original expected head/generation/UUID.
        let git = git.clone();
        let binding = expected.scope.source.clone();
        let workspace = expected.scope.workspace.clone();
        let roots = o.roots();
        // The original complete allocation receipt is retained by this
        // same journal owner. It is a comparison pin, not current proof.
        let original = o
            .journal(move |journal| {
                journal.state().attempts[&id]
                    .source
                    .as_ref()
                    .and_then(|s| s.operations.get("create"))
                    .and_then(|o| o.result.clone())
                    .context("result original workspace allocation absent")
            })
            .await?;
        let scope = expected.scope.clone();
        let (head, size) = tokio::task::spawn_blocking(move || -> Result<_> {
            roots.verify()?;
            let bytes = crate::source::workspace_bytes(&workspace.path)?;
            let value = git.query(
                &binding,
                "workspace.inspect",
                serde_json::json!({"workspace_id":workspace.workspace_id}),
            )?;
            let head = inspect_checkpoint_workspace(&value, &original, &scope)?;
            roots.verify()?;
            Ok((head, bytes))
        })
        .await??;
        o.verify()?;
        o.journal(move |journal| {
            let digest = observed.digest()?;
            journal.update_result(id, &digest, |h, _| {
                ensure!(
                    size <= h.scope.workspace_limit_bytes,
                    "stopped workspace exceeds original reservation"
                );
                let operation = h.scope.operations.checkpoint;
                let action = GitAction::Checkpoint {
                    workspace_id: h.scope.workspace.workspace_id,
                    generation: h.scope.workspace.generation,
                    head: head.clone(),
                };
                h.source.head = Some(head);
                h.source.workspace_bytes = Some(size);
                h.source.operations.insert(
                    KEY.into(),
                    PendingGitOperation {
                        operation_id: operation,
                        request: action.request(&h.scope.source, operation),
                        result: None,
                    },
                );
                Ok(())
            })
        })
        .await?
    };
    let operation = prepared.source.operations[KEY].clone();
    let expected_digest = prepared.digest()?;
    let roots = o.roots();
    let value = tokio::task::spawn_blocking(move || -> Result<_> {
        roots.verify()?;
        let value = git.exchange(&operation.request)?;
        ensure!(
            serde_json::to_vec(&value)?.len() <= MAX_GIT_REPLY_BYTES,
            "oversized result source reply; original operation retained"
        );
        roots.verify()?;
        Ok(value)
    })
    .await??;
    o.verify()?;
    o.journal(move |journal| {
        journal.update_result(id, &expected_digest, |h, _| {
            let op = h
                .source
                .operations
                .get_mut(KEY)
                .context("result source intent absent")?;
            ensure!(op.result.is_none(), "result source effect raced");
            op.result = Some(value.clone());
            h.source.checkpoint = Some(serde_json::from_value(value)?);
            Ok(())
        })?;
        Ok(())
    })
    .await
}
/// One step: the push (its intent journaled before any network I/O), or the
/// admission of the pushed checkpoint and its receipt. A retry after a crash or
/// a lost response repeats the same push and the same request.
pub(super) async fn push_step<O: StoppedOwner>(o: &O, client: &TransferClient) -> Result<bool> {
    let id = o.attempt_id();
    let mut history = snapshot(o).await?;
    if history.source.receipt.is_some() {
        return Ok(true);
    }
    if history.source.push.is_none() {
        let digest = history.digest()?;
        history = o
            .journal(move |journal| journal.update_result(id, &digest, |h, _| h.begin_push()))
            .await?;
    }
    let request = history.pushed_request()?;
    let digest = history.digest()?;
    if !history.source.push.as_ref().is_some_and(|p| p.pushed) {
        let (git, timeout, pool) = o.git().push_from(request.source.repository_id)?;
        let (roots, client) = (o.roots(), client.clone());
        o.verify()?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            roots.verify()?;
            let attempt = request.attempt.attempt_id;
            client.push_blocking(&git, &pool, attempt, &request.checkpoint, timeout)?;
            roots.verify()
        })
        .await??;
        o.verify()?;
        o.journal(move |journal| {
            journal.update_result(id, &digest, |h, _| {
                h.source.push.as_mut().context("push intent absent")?.pushed = true;
                Ok(())
            })
        })
        .await?;
        return Ok(false);
    }
    o.verify()?;
    let receipt = client.pushed(&request).await?.receipt;
    o.verify()?;
    o.journal(move |journal| {
        journal.update_result(id, &digest, |h, _| {
            h.source.receipt = receipt;
            h.validate()
        })
    })
    .await?;
    Ok(true)
}
/// aq-git's active inspection is an envelope containing the complete workspace
/// record. Compare it with the original allocation, then use the current HEAD;
/// dirty contents are precisely what this stopped checkpoint must preserve.
fn inspect_checkpoint_workspace(
    value: &serde_json::Value,
    original: &serde_json::Value,
    scope: &ScopeRecord,
) -> Result<aq_worker_protocol::ObjectId> {
    ensure!(
        original.is_object()
            && value.get("workspace") == Some(original)
            && original["workspace_id"] == scope.workspace.workspace_id.to_string()
            && original["generation"] == scope.workspace.generation
            && original["path"] == serde_json::to_value(&scope.workspace.path)?
            && original["repository_id"] == scope.source.repository_id.to_string()
            && original["binding_generation"] == scope.source.binding_generation
            && original["base"] == scope.source.base.hex
            && original["lifecycle"] == "active",
        "result workspace changed before checkpoint"
    );
    let head: aq_worker_protocol::ObjectId = serde_json::from_value(value["head"].clone())?;
    head.validate()?;
    Ok(head)
}
#[cfg(test)]
mod tests {
    use super::*;
    // Exact active envelope emitted by aq-git WorkspaceManager::workspace.inspect,
    // including its full allocation record (not the worker's three-field view).
    fn inspection() -> (ResultHistory, serde_json::Value, serde_json::Value) {
        let h = super::super::tests::history();
        let original = serde_json::json!({
            "format_version":1,"workspace_id":h.scope.workspace.workspace_id,
            "repository_id":h.scope.source.repository_id,
            "binding_generation":h.scope.source.binding_generation,
            "generation":h.scope.workspace.generation,"base":h.scope.source.base.hex,
            "path":h.scope.workspace.path,"lifecycle":"active",
            "allocating_operation":Uuid::new_v4(),"last_checkpoint":null,
            "retiring_operation":null,"config_digest":"a".repeat(64)
        });
        let value = serde_json::json!({"workspace":original,
            "head":{"algorithm":"sha1","hex":"b".repeat(40)},
            "clean":false,"status_encoding":"hex","status":"31204d20736565642e74787400"});
        (h, original, value)
    }
    #[test]
    fn checkpoint_inspection_accepts_actual_nested_dirty_workspace_and_current_head() {
        let (h, original, value) = inspection();
        let head = inspect_checkpoint_workspace(&value, &original, &h.scope).unwrap();
        assert_eq!(head.hex, "b".repeat(40));
        assert_ne!(
            head, h.scope.source.base,
            "preserve current committed HEAD too"
        );
        assert_eq!(value["clean"], false, "stopped edits need not be clean");
    }
    #[test]
    fn checkpoint_inspection_refuses_flat_inactive_and_changed_full_allocation() {
        let (h, original, value) = inspection();
        let mut flat = original.clone();
        flat["head"] = value["head"].clone();
        assert!(inspect_checkpoint_workspace(&flat, &original, &h.scope).is_err());
        for field in [
            "workspace_id",
            "repository_id",
            "binding_generation",
            "generation",
            "base",
            "path",
            "lifecycle",
            "allocating_operation",
            "last_checkpoint",
            "retiring_operation",
            "config_digest",
        ] {
            let mut changed = value.clone();
            changed["workspace"][field] = serde_json::json!("changed");
            assert!(
                inspect_checkpoint_workspace(&changed, &original, &h.scope).is_err(),
                "full original allocation field {field} must remain exact"
            );
        }
        let mut inactive = original;
        inactive["lifecycle"] = serde_json::json!("retired");
        let mut same = value;
        same["workspace"] = inactive.clone();
        assert!(inspect_checkpoint_workspace(&same, &inactive, &h.scope).is_err());
    }
    #[test]
    fn checkpoint_inspection_refuses_wrong_scope_or_malformed_current_head() {
        let (mut h, original, mut value) = inspection();
        h.scope.workspace.workspace_id = Uuid::new_v4();
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
        let (h, original, _) = inspection();
        // Use matching full allocation but an invalid object ID.
        value["workspace"] = original.clone();
        value["head"] = serde_json::json!({"algorithm":"sha1","hex":"not-an-oid"});
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
        value.as_object_mut().unwrap().remove("head");
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
    }
}
