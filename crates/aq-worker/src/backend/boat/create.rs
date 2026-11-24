//! Reconcile a durable creation intent before provisioning the VM and its container.
use super::*;
impl BoatBackend {
    pub(super) async fn create_sandbox(
        &mut self,
        r: &CreateRequest,
    ) -> Result<SandboxIdentity, BackendError> {
        let (size, template) = self.validate_creation(r)?;
        let mut rec = self.creation_record(r)?;
        if rec.backend_id.is_none() {
            self.list_owned(self.config.node_id).await?;
            rec = self.journal.get(&r.name).ok_or_else(unknown)?.clone();
        }
        if rec.backend_id.is_none() {
            let vm = self.provision_vm(r, size, &template).await?;
            let id = vm["id"]
                .as_str()
                .filter(|s| config::safe_id(s))
                .ok_or_else(unknown)?
                .to_owned();
            rec.backend_id = Some(id);
            self.save(rec.clone())?;
            let entry = self.entry(&vm)?;
            self.observe_vm_time(&vm, &entry)?;
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
        self.wait_ready(&s).await?;
        self.prepare_container(r, &s, &template).await?;
        rec.container = true;
        self.save(rec)?;
        Ok(s)
    }

    fn validate_creation(
        &self,
        r: &CreateRequest,
    ) -> Result<(&'static str, BoatTemplate), BackendError> {
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
        if !r.mount_aliases.is_empty() || r.agent_channel {
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
        Ok((size, t))
    }

    fn creation_record(&mut self, r: &CreateRequest) -> Result<Record, BackendError> {
        Ok(if let Some(old) = self.journal.get(&r.name) {
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
        })
    }

    async fn provision_vm(
        &self,
        r: &CreateRequest,
        size: &str,
        t: &BoatTemplate,
    ) -> Result<Value, BackendError> {
        let ttl = policy::ttl(
            r.hard_deadline_ms,
            now()?.saturating_add(self.config.api_timeout_ms),
        )?;
        let mut body = json!({
            "name": r.name, "type": size, "ttlSeconds": ttl,
            "noEnv": true, "env": {}, "monitoring": false, "snapshots": true
        });
        if let (Some(saved_name), Some(artifact)) = (&t.saved_snapshot, &t.snapshot_id) {
            let saved = self
                .api
                .request(Method::GET, &format!("/saved-snapshots/{saved_name}"))
                .json()
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
            .request(Method::POST, "/sandboxes")
            .body(body)
            .idempotency(&r.name)
            .json()
            .await?;
        Ok(v["sandbox"].clone())
    }

    async fn wait_ready(&self, s: &SandboxIdentity) -> Result<(), BackendError> {
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            let vm = self.get(&s.backend_id).await?;
            self.entry(&vm)?;
            if matches!(vm["state"].as_str(), Some("ready" | "running" | "idle")) {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }

    async fn prepare_container(
        &mut self,
        r: &CreateRequest,
        s: &SandboxIdentity,
        t: &BoatTemplate,
    ) -> Result<(), BackendError> {
        // Reassert the TTL after provisioning, but never extend past the hard bound.
        self.extend_auto_stop(s, r.hard_deadline_ms).await?;
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
        Ok(())
    }
}
