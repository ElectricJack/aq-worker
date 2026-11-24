//! Journal deletion operations so retries reuse the provider operation.
use super::*;
impl BoatBackend {
    pub(super) async fn destroy_inner(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<StoppedWriterProof, BackendError> {
        let mut r = match self.record(s) {
            Ok(r) => r,
            Err(BackendError::NotFound) => return self.destroy_orphan(s).await,
            Err(e) => return Err(e),
        };
        if r.destroyed {
            return Ok(StoppedWriterProof::destroyed(s.clone()));
        }
        if r.deletion.is_none() {
            self.scoped_entry(s).await?;
            self.stop_inner(s).await?;
            r.deletion = Some(self.request_deletion(s).await?);
            self.save(r.clone())?;
        }
        // The permanent-delete operation covers all automatic snapshots and
        // content records. Mere disappearance from GET/list occurs earlier and
        // cannot attest storage erasure. Saved build templates are independent.
        let operation = r.deletion.as_ref().ok_or_else(unknown)?;
        let proof = self.wait_deleted(s, operation).await?;
        r.destroyed = true;
        self.save(r)?;
        Ok(proof)
    }
    async fn destroy_orphan(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<StoppedWriterProof, BackendError> {
        let mut orphan = self.orphan_for_deletion(s).await?;
        if orphan.destroyed {
            return Ok(StoppedWriterProof::destroyed(s.clone()));
        }
        if orphan.operation.is_none() {
            self.stop_inner(s).await?;
            orphan.operation = Some(self.request_deletion(s).await?);
            self.journal.set_orphan(orphan.clone())?;
        }
        let proof = self
            .wait_deleted(s, orphan.operation.as_deref().ok_or_else(unknown)?)
            .await?;
        orphan.destroyed = true;
        self.journal.set_orphan(orphan)?;
        Ok(proof)
    }
    async fn request_deletion(&self, s: &SandboxIdentity) -> Result<String, BackendError> {
        let accepted = self
            .api
            .request(Method::DELETE, &format!("/sandboxes/{}", s.backend_id))
            .json()
            .await?;
        let op = &accepted["operation"];
        if op["kind"] != "sandbox" || op["targetId"] != s.backend_id {
            return Err(unknown());
        }
        let id = op["id"]
            .as_str()
            .filter(|id| config::safe_id(id))
            .ok_or_else(unknown)?
            .to_owned();
        Ok(id)
    }

    async fn orphan_for_deletion(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<journal::OrphanDeletion, BackendError> {
        self.journal.check()?;
        let local = s.kind == BackendKind::Boat && s.owner.node_id == self.config.node_id;
        if !local || !s.owner.boot_id.is_nil() {
            return Err(BackendError::Conflict);
        }
        Ok(if let Some(o) = self.journal.orphan(&s.backend_id) {
            if o.entry.sandbox != *s {
                return Err(BackendError::Conflict);
            }
            o.clone()
        } else {
            journal::OrphanDeletion {
                entry: self.scoped_entry(s).await?,
                operation: None,
                destroyed: false,
            }
        })
    }
}
