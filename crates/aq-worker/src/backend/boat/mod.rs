//! Boat VM placement. All authority and credentials remain on the worker node.
//! Names delimit discovery; the fsynced journal binds boot, digest and deadline.
mod api;
mod config;
mod journal;
mod policy;
mod vm_time;
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
        if s.kind != BackendKind::Boat
            || s.owner.node_id != self.config.node_id
            || !config::safe_id(&s.backend_id)
        {
            return Err(BackendError::Conflict);
        }
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
    async fn get(&self, id: &str) -> Result<Value, BackendError> {
        Ok(self
            .api
            .json(Method::GET, &format!("/sandboxes/{id}"), None, None)
            .await?["sandbox"]
            .clone())
    }
    /// Org scope: billed to the attempt org. Personal scope (no organization):
    /// unbilled to any org and owned by this key's user, so delete-own applies.
    fn in_scope(&self, vm: &Value) -> bool {
        if self.config.personal() {
            vm["team"].is_null() && vm["access"] == "owner"
        } else {
            vm["team"]["id"] == self.config.attempt_org.as_str()
        }
    }
    fn entry(&self, vm: &Value) -> Result<InventoryEntry, BackendError> {
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
    async fn scoped_entry(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        if s.kind != BackendKind::Boat
            || s.owner.node_id != self.config.node_id
            || !config::safe_id(&s.backend_id)
        {
            return Err(BackendError::Conflict);
        }
        let vm = self.get(&s.backend_id).await?;
        let e = self.entry(&vm)?;
        if &e.sandbox != s {
            return Err(BackendError::Conflict);
        }
        self.observe_vm_time(&vm, &e)?;
        Ok(e)
    }
    async fn command(&self, id: &str, command: String) -> Result<String, BackendError> {
        let v = self
            .api
            .json(
                Method::POST,
                &format!("/sandboxes/{id}/commands"),
                Some(json!({"command":command,"timeoutSeconds":api::COMMAND_TIMEOUT_SECS})),
                None,
            )
            .await?;
        if v["exitCode"] != 0 || v["timedOut"] == true || v["success"] == false {
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
        self.api
            .json(
                Method::PATCH,
                &format!("/sandboxes/{}", s.backend_id),
                Some(json!({"ttlSeconds":policy::ttl(deadline,now()?.saturating_add(self.config.api_timeout_ms))?})),
                None,
            )
            .await?;
        Ok(())
    }
    /// Artifact collection works after archive without resuming any writer.
    pub async fn read_work_file(
        &mut self,
        s: &SandboxIdentity,
        path: &str,
    ) -> Result<Vec<u8>, BackendError> {
        let r = self.record(s)?;
        if path.is_empty()
            || path.starts_with('/')
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || !path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
        {
            return Err(BackendError::InvalidRequest("invalid work file"));
        }
        let e = self.scoped_entry(s).await?;
        if e.state == SandboxState::Archived {
            let v = self
                .api
                .json(
                    Method::GET,
                    &format!("/sandboxes/{}/snapshots/latest", s.backend_id),
                    None,
                    None,
                )
                .await?;
            let id = v["snapshot"]["id"]
                .as_str()
                .filter(|s| config::safe_id(s))
                .ok_or_else(unknown)?;
            let (status, b) = self
                .api
                .raw(
                    Method::GET,
                    &format!(
                        "/snapshots/{id}/files?path={}",
                        api::encode(&format!(
                            "__dockervol__/{}-work/_data/{path}",
                            r.request.name
                        ))
                    ),
                    None,
                    None,
                )
                .await?;
            if status != 200 {
                return Err(unknown());
            }
            Ok(b)
        } else {
            let out = self
                .command(
                    &s.backend_id,
                    format!(
                        "sudo docker cp {}:/aq/work/{path} - | tar -xO | base64 -w0",
                        r.request.name
                    ),
                )
                .await?;
            STANDARD.decode(out.trim()).map_err(|_| unknown())
        }
    }
    async fn stop_inner(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<StoppedWriterProof, BackendError> {
        let e = self.scoped_entry(s).await?;
        if e.state != SandboxState::Archived {
            self.api
                .json(
                    Method::POST,
                    &format!("/sandboxes/{}/stop", s.backend_id),
                    Some(json!({"force":false})),
                    None,
                )
                .await?;
        }
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            let e = self.scoped_entry(s).await?;
            if let Confirmation::Confirmed(p) = e.writer_proof() {
                self.replay_vm_time().await?;
                return Ok(p);
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }
    async fn destroy_inner(
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
            let accepted = self
                .api
                .json(
                    Method::DELETE,
                    &format!("/sandboxes/{}", s.backend_id),
                    None,
                    None,
                )
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
            r.deletion = Some(id);
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
        self.journal.check()?;
        if s.kind != BackendKind::Boat
            || s.owner.node_id != self.config.node_id
            || !s.owner.boot_id.is_nil()
        {
            return Err(BackendError::Conflict);
        }
        let mut orphan = if let Some(o) = self.journal.orphan(&s.backend_id) {
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
        };
        if orphan.destroyed {
            return Ok(StoppedWriterProof::destroyed(s.clone()));
        }
        if orphan.operation.is_none() {
            self.stop_inner(s).await?;
            let accepted = self
                .api
                .json(
                    Method::DELETE,
                    &format!("/sandboxes/{}", s.backend_id),
                    None,
                    None,
                )
                .await?;
            let op = &accepted["operation"];
            if op["kind"] != "sandbox" || op["targetId"] != s.backend_id {
                return Err(unknown());
            }
            orphan.operation = Some(
                op["id"]
                    .as_str()
                    .filter(|id| config::safe_id(id))
                    .ok_or_else(unknown)?
                    .to_owned(),
            );
            self.journal.set_orphan(orphan.clone())?;
        }
        let proof = self
            .wait_deleted(s, orphan.operation.as_deref().ok_or_else(unknown)?)
            .await?;
        orphan.destroyed = true;
        self.journal.set_orphan(orphan)?;
        Ok(proof)
    }
    async fn wait_deleted(
        &self,
        s: &SandboxIdentity,
        operation: &str,
    ) -> Result<StoppedWriterProof, BackendError> {
        let path = format!("/sandboxes/{}/snapshots", s.backend_id);
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            let v = self
                .api
                .json(
                    Method::GET,
                    &format!("/deletion-operations/{operation}"),
                    None,
                    None,
                )
                .await?;
            let op = &v["operation"];
            if op["id"] != *operation || op["kind"] != "sandbox" || op["targetId"] != s.backend_id {
                return Err(unknown());
            }
            let gone = match self.get(&s.backend_id).await {
                Err(BackendError::NotFound) => true,
                Ok(_) => false,
                Err(e) => return Err(e),
            };
            let snapshots_gone = match self.api.json(Method::GET, &path, None, None).await {
                Ok(v) => v["snapshots"].as_array().is_some_and(Vec::is_empty),
                Err(BackendError::NotFound) => true,
                Err(e) => return Err(e),
            };
            if op["status"] == "completed" && gone && snapshots_gone {
                return Ok(StoppedWriterProof::destroyed(s.clone()));
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
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
        self.journal.check()?;
        r.validate()?;
        if self
            .journal
            .fact(r.owner.attempt_id)
            .is_some_and(|f| f.fact.is_not_created())
        {
            return Err(BackendError::Conflict);
        }
        policy::ttl(
            r.hard_deadline_ms,
            now()?.saturating_add(self.config.api_timeout_ms),
        )?;
        if r.owner.node_id != self.config.node_id {
            return Err(BackendError::Conflict);
        }
        for cap in &r.required_capabilities {
            if !self.capabilities().contains(cap) {
                return Err(BackendError::UnsupportedCapability(*cap));
            }
        }
        if !r.mount_aliases.is_empty() {
            return Err(BackendError::InvalidRequest(
                "boat has no host mount aliases",
            ));
        }
        let size = policy::size(r.resources.ok_or(BackendError::InvalidRequest(
            "boat requires resource reservation",
        ))?)?;
        let t = self
            .config
            .templates
            .iter()
            .find(|t| t.digest == r.template_digest)
            .ok_or(BackendError::InvalidRequest("unmaterialized boat template"))?
            .clone();
        let mut rec = if let Some(old) = self.journal.get(&r.name) {
            if old.request != *r || old.destroyed {
                return Err(BackendError::Conflict);
            }
            old.clone()
        } else {
            let rec = Record {
                request: r.clone(),
                backend_id: None,
                container: false,
                material: false,
                start_intent: false,
                destroyed: false,
                deletion: None,
            };
            self.save(rec.clone())?;
            rec
        };
        if rec.backend_id.is_none() {
            self.list_owned(self.config.node_id).await?;
            rec = self.journal.get(&r.name).ok_or_else(unknown)?.clone();
        }
        if rec.backend_id.is_none() {
            let mut body = json!({"name":r.name,"type":size,"ttlSeconds":policy::ttl(r.hard_deadline_ms,now()?.saturating_add(self.config.api_timeout_ms))?,"noEnv":true,"env":{},"monitoring":false,"snapshots":true});
            if let (Some(saved_name), Some(artifact)) = (&t.saved_snapshot, &t.snapshot_id) {
                let saved = self
                    .api
                    .json(
                        Method::GET,
                        &format!("/saved-snapshots/{saved_name}"),
                        None,
                        None,
                    )
                    .await?;
                if saved["snapshot"]["status"] != "ready"
                    || saved["snapshot"]["snapshotId"] != artifact.as_str()
                {
                    return Err(BackendError::InvalidRequest(
                        "boat template artifact changed or not ready",
                    ));
                }
                body["from"] = json!(saved_name);
            }
            // Personal sandboxes are always private; boat refuses `visibility`
            // semantics there (`not_org_sandbox`), so only orgs set it.
            if !self.config.personal() {
                body["visibility"] = json!("private");
            }
            let v = self
                .api
                .json(Method::POST, "/sandboxes", Some(body), Some(&r.name))
                .await?;
            let id = v["sandbox"]["id"]
                .as_str()
                .filter(|s| config::safe_id(s))
                .ok_or_else(unknown)?
                .to_owned();
            rec.backend_id = Some(id);
            self.save(rec.clone())?;
            let entry = self.entry(&v["sandbox"])?;
            self.observe_vm_time(&v["sandbox"], &entry)?;
            self.replay_vm_time().await?;
        }
        let s = SandboxIdentity {
            kind: BackendKind::Boat,
            backend_id: rec.backend_id.clone().ok_or_else(unknown)?,
            owner: r.owner.clone(),
        };
        if rec.container {
            self.scoped_entry(&s).await?;
            self.replay_vm_time().await?;
            return Ok(s);
        }
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            let vm = self.get(&s.backend_id).await?;
            self.entry(&vm)?;
            if matches!(vm["state"].as_str(), Some("ready" | "running" | "idle")) {
                break;
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
        // Reassert the TTL after provisioning, but never extend past the hard bound.
        self.extend_auto_stop(&s, r.hard_deadline_ms).await?;
        // Firewall changes are not persisted by boat snapshots. Apply per fork;
        // keep established control traffic and agent port 8911 available.
        self.command(&s.backend_id, "set -eu; for t in iptables ip6tables; do sudo $t -C INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null || sudo $t -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT; sudo $t -C INPUT -i lo -j ACCEPT 2>/dev/null || sudo $t -A INPUT -i lo -j ACCEPT; sudo $t -C INPUT -p tcp --dport 8911 -j ACCEPT 2>/dev/null || sudo $t -A INPUT -p tcp --dport 8911 -j ACCEPT; sudo $t -P INPUT DROP; done".into()).await?;
        if t.is_stock() {
            // Stock image: anonymous public pull by the C3b-scanned digest; no
            // registry credential or Docker config is forwarded.
            self.command(&s.backend_id, format!("set -eu; for i in $(seq 1 30); do sudo docker info >/dev/null 2>&1 && break; sleep 1; done; sudo docker pull -q {} >/dev/null", t.image)).await?;
        }
        self.command(&s.backend_id, format!("set -eu; for i in $(seq 1 60); do sudo docker info >/dev/null 2>&1 && break; sleep 1; done; sudo docker image inspect --format '{{{{range .RepoDigests}}}}{{{{println .}}}}{{{{end}}}}' {} | grep -Fx '{}' >/dev/null", t.image, t.image)).await?;
        self.command(
            &s.backend_id,
            policy::create_container(r, &t.image, self.config.runner_command.as_deref()),
        )
        .await?;
        rec.container = true;
        self.save(rec)?;
        Ok(s)
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
        if !r.container || r.material || r.start_intent {
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
        self.api.json(Method::PUT,&format!("/sandboxes/{}/files",s.backend_id),Some(json!({"path":format!("{dir}/material.tar"),"content":STANDARD.encode(&*tar),"encoding":"base64"})),None).await?;
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
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        self.journal.check()?;
        if node != self.config.node_id {
            return Err(BackendError::Conflict);
        }
        // Deliver even if the provider inventory is currently unavailable, or
        // all reported VMs have already been destroyed.
        self.replay_vm_time().await?;
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut result = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let mut path = format!("/sandboxes?namePrefix={}&limit=200", policy::prefix(node));
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={}", api::encode(c)));
            }
            let page = self.api.json(Method::GET, &path, None, None).await?;
            for vm in page["sandboxes"].as_array().ok_or_else(unknown)? {
                if !self.in_scope(vm)
                    || vm["name"]
                        .as_str()
                        .is_none_or(|s| policy::attempt(s, node).is_none())
                {
                    continue;
                }
                let entry = self.entry(vm)?;
                if !ids.insert(entry.sandbox.backend_id.clone()) {
                    return Err(unknown());
                }
                if let Some(mut r) = self.journal.get(&entry.name).cloned()
                    && r.backend_id.is_none()
                {
                    r.backend_id = Some(entry.sandbox.backend_id.clone());
                    self.save(r)?;
                }
                self.observe_vm_time(vm, &entry)?;
                result.push(entry);
            }
            match page["pageInfo"]["hasMore"].as_bool() {
                Some(false) => break,
                Some(true) => {
                    let c = page["pageInfo"]["nextCursor"]
                        .as_str()
                        .filter(|c| !c.is_empty())
                        .ok_or_else(unknown)?
                        .to_owned();
                    if !seen.insert(c.clone()) || seen.len() > 10_000 {
                        return Err(unknown());
                    }
                    cursor = Some(c);
                }
                None => return Err(unknown()),
            }
        }
        self.replay_vm_time().await?;
        Ok(result)
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
