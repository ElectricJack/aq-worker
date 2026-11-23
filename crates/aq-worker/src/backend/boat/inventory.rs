//! Resolve provider identities and interpret the state of a single sandbox.
use super::*;
impl BoatBackend {
    pub(super) async fn get(&self, id: &str) -> Result<Value, BackendError> {
        Ok(self
            .api
            .request(Method::GET, &format!("/sandboxes/{id}"))
            .json()
            .await?["sandbox"]
            .clone())
    }
    /// Org scope: billed to the attempt org. Personal scope (no organization):
    /// unbilled to any org and owned by this key's user, so delete-own applies.
    pub(super) fn in_scope(&self, vm: &Value) -> bool {
        if self.config.personal() {
            vm["team"].is_null() && vm["access"] == "owner"
        } else {
            vm["team"]["id"] == self.config.attempt_org.as_str()
        }
    }
    pub(super) fn entry(&self, vm: &Value) -> Result<InventoryEntry, BackendError> {
        self.journal.check()?;
        let id = vm["id"]
            .as_str()
            .filter(|id| config::safe_id(id))
            .ok_or_else(unknown)?;
        let name = vm["name"].as_str().ok_or_else(unknown)?;
        let attempt = policy::attempt(name, self.config.node_id).ok_or(BackendError::Conflict)?;
        if !self.in_scope(vm) {
            return Err(BackendError::Conflict);
        }
        let r = self.journal.get(name);
        if r.is_some_and(|r| {
            r.backend_id.as_deref().is_some_and(|stored| stored != id) || r.destroyed
        }) {
            return Err(BackendError::Conflict);
        }
        let archived = vm["state"] == "archived"
            && vm["snapshotAvailable"] == true
            && vm["stop"]["status"]
                .as_str()
                .is_none_or(|s| s == "completed");
        Ok(InventoryEntry {
            sandbox: SandboxIdentity {
                kind: BackendKind::Boat,
                backend_id: id.into(),
                owner: r.map(|r| r.request.owner.clone()).unwrap_or(Ownership {
                    node_id: self.config.node_id,
                    boot_id: Uuid::nil(),
                    attempt_id: attempt,
                }),
            },
            name: name.into(),
            template_digest: r
                .map(|r| r.request.template_digest.clone())
                .unwrap_or(TemplateDigest::parse(format!("sha256:{}", "0".repeat(64)))?),
            hard_deadline_ms: r.map(|r| r.request.hard_deadline_ms).unwrap_or(1),
            state: if archived {
                SandboxState::Archived
            } else if matches!(vm["state"].as_str(), Some("ready" | "running" | "idle")) {
                SandboxState::Running
            } else {
                SandboxState::Unknown
            },
            running_processes: archived.then_some(0),
            disk_present: true,
        })
    }
    pub(super) async fn scoped_entry(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<InventoryEntry, BackendError> {
        self.validate_identity(s)?;
        let vm = self.get(&s.backend_id).await?;
        let e = self.entry(&vm)?;
        if &e.sandbox != s {
            return Err(BackendError::Conflict);
        }
        self.observe_vm_time(&vm, &e)?;
        Ok(e)
    }
    pub(super) async fn inspect_sandbox(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<InventoryEntry, BackendError> {
        let r = match self.record(s) {
            Ok(r) => r,
            Err(BackendError::NotFound) => {
                if let Some(o) = self.journal.orphan(&s.backend_id) {
                    if o.entry.sandbox != *s {
                        return Err(BackendError::Conflict);
                    }
                    if o.destroyed {
                        let mut e = o.entry.clone();
                        e.state = SandboxState::Destroyed;
                        e.running_processes = Some(0);
                        e.disk_present = false;
                        return Ok(e);
                    }
                }
                let entry = self.scoped_entry(s).await?;
                self.replay_vm_time().await?;
                return Ok(entry);
            }
            Err(e) => return Err(e),
        };
        if r.destroyed {
            return Ok(InventoryEntry {
                sandbox: s.clone(),
                name: r.request.name,
                template_digest: r.request.template_digest,
                hard_deadline_ms: r.request.hard_deadline_ms,
                state: SandboxState::Destroyed,
                running_processes: Some(0),
                disk_present: false,
            });
        }
        let entry = self.scoped_entry(s).await?;
        self.replay_vm_time().await?;
        Ok(entry)
    }
}
