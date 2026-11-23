//! Boat VM placement. All authority and credentials remain on the worker node.
//! Names delimit discovery; the fsynced journal binds boot, digest and deadline.
mod api;
mod config;
mod create;
mod deletion;
mod inventory;
mod journal;
mod lifecycle_proofs;
mod listing;
mod policy;
mod vm_time;
mod work_files;
use super::*;
use api::{Api, unknown};
use base64::{Engine, engine::general_purpose::STANDARD};
pub use config::{BoatConfig, BoatKey, BoatTemplate};
use journal::{Journal, Record};
pub use policy::AUTO_STOP_MARGIN_MS;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
pub use vm_time::{VmSize, VmTimeFact};

pub struct BoatBackend {
    config: BoatConfig,
    api: Api,
    journal: Journal,
    vm_time_client: Option<crate::client::Client>,
}
impl BoatBackend {
    pub fn connect(config: BoatConfig, key: BoatKey) -> Result<Self, BackendError> {
        config.validate()?;
        let journal = Journal::open(&config)?;
        let api = Api::new(&config, key)?;
        Ok(Self {
            config,
            api,
            journal,
            vm_time_client: None,
        })
    }
    fn record(&self, s: &SandboxIdentity) -> Result<Record, BackendError> {
        self.journal.check()?;
        self.validate_identity(s)?;
        let r = self
            .journal
            .by_id(&s.backend_id)
            .ok_or(BackendError::NotFound)?;
        if r.request.owner != s.owner {
            return Err(BackendError::Conflict);
        }
        Ok(r.clone())
    }
    fn save(&mut self, r: Record) -> Result<(), BackendError> {
        self.journal.set(r.request.name.clone(), r)
    }
    fn validate_identity(&self, s: &SandboxIdentity) -> Result<(), BackendError> {
        let local = s.kind == BackendKind::Boat && s.owner.node_id == self.config.node_id;
        if local && config::safe_id(&s.backend_id) {
            Ok(())
        } else {
            Err(BackendError::Conflict)
        }
    }

    async fn command(&self, id: &str, command: String) -> Result<String, BackendError> {
        let v = self
            .api
            .request(Method::POST, &format!("/sandboxes/{id}/commands"))
            .body(json!({"command":command,"timeoutSeconds":api::COMMAND_TIMEOUT_SECS}))
            .json()
            .await?;
        let exited = v["exitCode"] == 0 && v["timedOut"] != true;
        if !exited || v["success"] == false {
            return Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure));
        }
        v["stdout"].as_str().map(str::to_owned).ok_or_else(unknown)
    }
    /// Upper bound stays the original assignment deadline. No AI endpoint is used.
    pub async fn extend_auto_stop(
        &mut self,
        s: &SandboxIdentity,
        deadline: u64,
    ) -> Result<(), BackendError> {
        let r = self.record(s)?;
        if deadline > r.request.hard_deadline_ms {
            return Err(BackendError::InvalidRequest("cannot extend hard deadline"));
        }
        self.scoped_entry(s).await?;
        let ttl = policy::ttl(deadline, now()?.saturating_add(self.config.api_timeout_ms))?;
        self.api
            .request(Method::PATCH, &format!("/sandboxes/{}", s.backend_id))
            .body(json!({ "ttlSeconds": ttl }))
            .json()
            .await?;
        Ok(())
    }
}
fn now() -> Result<u64, BackendError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| unknown())
}
fn confirmation(
    result: Result<StoppedWriterProof, BackendError>,
) -> Confirmation<StoppedWriterProof> {
    match result {
        Ok(p) => Confirmation::Confirmed(p),
        Err(BackendError::Uncertain(r)) => Confirmation::Unconfirmed(r),
        Err(BackendError::NotFound) => Confirmation::Unconfirmed(UnconfirmedReason::NotFound),
        Err(BackendError::Conflict) => Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized),
        Err(_) => Confirmation::Unconfirmed(UnconfirmedReason::BackendFailure),
    }
}
impl PlacementBackend for BoatBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Boat
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::VmIsolation,
            Capability::SuspendSnapshot,
            Capability::PersistentDiskDuringAttempt,
        ]
    }
    async fn create(&mut self, r: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        self.create_sandbox(r).await
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        material: PrivateMaterial,
    ) -> Result<(), BackendError> {
        let mut r = self.record(s)?;
        if now()? >= r.request.hard_deadline_ms {
            return Err(BackendError::InvalidRequest("boat hard deadline expired"));
        }
        self.scoped_entry(s).await?;
        let ready = r.container && !r.material;
        if !ready || r.start_intent {
            return Err(BackendError::InvalidState);
        }
        let dir = format!("/tmp/{}-private", r.request.name);
        self.command(&s.backend_id, format!("umask 077; mkdir -m 700 {dir}"))
            .await?;
        let tar = super::docker_local::tar::material_archive(
            "/aq/private/material",
            material.expose_for_control_channel(),
            0,
            0,
        )?;
        self.api
            .request(Method::PUT, &format!("/sandboxes/{}/files", s.backend_id))
            .body(json!({
                "path": format!("{dir}/material.tar"),
                "content": STANDARD.encode(&*tar),
                "encoding": "base64"
            }))
            .json()
            .await?;
        self.command(
            &s.backend_id,
            format!(
                "trap 'rm -rf {dir}' EXIT; sudo docker cp -a - {}:/ < {dir}/material.tar",
                r.request.name
            ),
        )
        .await?;
        r.material = true;
        self.save(r)
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        let mut r = self.record(s)?;
        if now()? >= r.request.hard_deadline_ms {
            return Err(BackendError::InvalidRequest("boat hard deadline expired"));
        }
        self.scoped_entry(s).await?;
        if !r.material || r.start_intent {
            return Err(BackendError::InvalidState);
        }
        r.start_intent = true;
        self.save(r.clone())?;
        self.command(
            &s.backend_id,
            format!("sudo docker start {}", r.request.name),
        )
        .await?;
        Ok(())
    }
    async fn inspect(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        self.inspect_sandbox(s).await
    }
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        self.list_inventory(node).await
    }
    async fn record_not_created(
        &mut self,
        request: &CreateRequest,
        at_ms: u64,
    ) -> Result<(), BackendError> {
        self.not_created(request, at_ms).await
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        confirmation(self.stop_inner(s).await)
    }
    async fn archive(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        self.stop(s).await
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        if !a.authorizes(s) {
            return Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized);
        }
        confirmation(self.destroy_inner(s).await)
    }
}
