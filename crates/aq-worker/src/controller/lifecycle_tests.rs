use super::tests::{config, now_ms, request};
use super::*;
use aq_admission_sandbox::sandbox::DelegatedPermit;
use aq_session_protocol::LaunchManifest;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

struct Provider {
    root: PathBuf,
    bad: bool,
    expired: bool,
}
impl PermitProvider for Provider {
    type Request = ();
    async fn register(&mut self, r: &Registration) -> Result<()> {
        r.validate()?;
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json"))?)?;
        assert_eq!(
            v["state"]["registration_operation_id"],
            r.registration.operation_id.to_string()
        );
        Ok(())
    }
    async fn issue(
        &mut self,
        _: &(),
        sandbox: &SandboxIdentity,
        manifest: &LaunchManifest,
    ) -> Result<IssuedMaterial> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json"))?)?;
        let row = &v["state"]["attempts"][sandbox.owner.attempt_id.to_string()];
        assert_eq!(row["phase"], "issue_intent");
        assert_eq!(row["sandbox"]["backend_id"], sandbox.backend_id);
        assert_eq!(manifest.bindings.attempt_id, sandbox.owner.attempt_id);
        let permit: DelegatedPermit = serde_json::from_value(serde_json::json!({
            "schema_version":1,"permit_id":Uuid::new_v4(),"credential":"opaque_secret_01234567890123456789012345",
            "binding":{"attempt_id":if self.bad {Uuid::new_v4()} else {sandbox.owner.attempt_id},"grant_generation":manifest.bindings.assignment_generation,"sandbox_id":Uuid::new_v4(),"node":{"node_id":sandbox.owner.node_id,"boot_id":sandbox.owner.boot_id},"channel_id":Uuid::new_v4()},
            "scope":"preparation","issued_at_ms":now_ms()-10,"expires_at_ms":if self.expired { serde_json::json!(now_ms()-1) } else { row["request"]["hard_deadline_ms"].clone() },"hard_deadline_ms":row["request"]["hard_deadline_ms"]
        }))?;
        // An opaque fake runner payload, not a proposed production wire format.
        let payload = PrivateMaterial::new(b"fake-runner-private-payload".to_vec());
        Ok(IssuedMaterial { permit, payload })
    }
}
struct ObservedBackend {
    inner: FakeBackend,
    root: PathBuf,
}
impl PlacementBackend for ObservedBackend {
    fn kind(&self) -> BackendKind {
        self.inner.kind()
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.inner.capabilities()
    }
    async fn create(
        &mut self,
        r: &CreateRequest,
    ) -> std::result::Result<SandboxIdentity, BackendError> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap();
        let row = &v["state"]["attempts"][r.owner.attempt_id.to_string()];
        assert_eq!(row["phase"], "create_intent");
        let persisted: CreateRequest = serde_json::from_value(row["create"].clone()).unwrap();
        assert_eq!(persisted, *r);
        self.inner.create(r).await
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> std::result::Result<(), BackendError> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap();
        assert_eq!(
            v["state"]["attempts"][s.owner.attempt_id.to_string()]["phase"],
            "material_intent"
        );
        assert_eq!(
            m.expose_for_control_channel(),
            b"fake-runner-private-payload"
        );
        self.inner.deliver_private_material(s, m).await
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> std::result::Result<(), BackendError> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap();
        assert_eq!(
            v["state"]["attempts"][s.owner.attempt_id.to_string()]["phase"],
            "start_intent"
        );
        self.inner.start_runner(s).await
    }
    async fn inspect(
        &mut self,
        s: &SandboxIdentity,
    ) -> std::result::Result<InventoryEntry, BackendError> {
        self.inner.inspect(s).await
    }
    async fn list_owned(
        &mut self,
        n: Uuid,
    ) -> std::result::Result<Vec<InventoryEntry>, BackendError> {
        self.inner.list_owned(n).await
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap();
        assert_eq!(
            v["state"]["attempts"][s.owner.attempt_id.to_string()]["phase"],
            "stop_intent"
        );
        self.inner.stop(s).await
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap();
        let row = &v["state"]["attempts"][s.owner.attempt_id.to_string()];
        assert_eq!(row["phase"], "destroy_intent");
        let persisted: DestroyAuthorization =
            serde_json::from_value(row["destroy_authorization"].clone()).unwrap();
        assert_eq!(persisted, *a);
        self.inner.destroy(s, a).await
    }
    async fn disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        self.inner.disk_usage(preserved).await
    }
}
fn open(dir: &tempfile::TempDir, bad: bool) -> Controller<ObservedBackend, Provider> {
    let root = dir.path().join("node");
    Controller::open(
        &root,
        Uuid::from_u128(1),
        config(),
        ObservedBackend {
            inner: FakeBackend::default(),
            root: root.clone(),
        },
        Provider {
            root: root.clone(),
            bad,
            expired: false,
        },
    )
    .unwrap()
}
async fn accept(c: &mut Controller<ObservedBackend, Provider>) -> Uuid {
    c.register().await.unwrap();
    let id = c.reserve(request(c.identity(), 10)).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    id
}
#[tokio::test]
async fn journal_records_intent_before_backend_create() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Running);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    let entry = c.inspect(id).await.unwrap();
    assert_eq!(entry.state, SandboxState::Running);
    assert_eq!(
        entry.template_digest.as_str(),
        format!(
            "sha256:{}",
            c.state().attempts[&id]
                .request
                .manifest
                .harness
                .environment
                .digest
                .as_str()
        )
    );
    assert_eq!(entry.sandbox.owner.boot_id, c.identity().boot_id);
    let journal = fs::read_to_string(dir.path().join("node/journal.json")).unwrap();
    assert!(!journal.contains("opaque_secret"));
    assert!(!journal.contains("fake-runner-private-payload"));
    let private = dir.path().join(format!("node/permit-{id}.json"));
    assert_eq!(
        fs::metadata(&private).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(private)
            .unwrap()
            .contains("opaque_secret")
    );
    let frozen: LaunchManifest = serde_json::from_slice(
        &fs::read(dir.path().join(format!("node/manifest-{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(frozen, c.state().attempts[&id].request.manifest);
}
#[tokio::test]
async fn ambiguous_create_is_not_retried_or_released() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.backend.inner.inject(Fault::CrashAfterCreate);
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.state().attempts[&id].phase, Phase::CreateIntent);
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(
        c.backend
            .list_owned(c.identity().node_id)
            .await
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn wrong_permit_cannot_deliver_or_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, true);
    let id = accept(&mut c).await;
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.state().attempts[&id].phase, Phase::IssueIntent);
    assert_eq!(c.inspect(id).await.unwrap().state, SandboxState::Created);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(!dir.path().join(format!("node/permit-{id}.json")).exists());
}
#[tokio::test]
async fn stop_and_destroy_require_confirmation_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    c.backend.inner.inject(Fault::StopFailure);
    assert!(!c.stop(id).await.unwrap().is_confirmed());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(c.stop(id).await.unwrap().is_confirmed());
    assert_eq!(c.inventory().reserved_sandboxes, 0);
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    let auth = DestroyAuthorization::retained(s, "retained-receipt-1").unwrap();
    c.backend.inner.inject(Fault::DestroyFailure);
    assert!(!c.destroy(id, &auth).await.unwrap().is_confirmed());
    assert_ne!(c.state().attempts[&id].phase, Phase::Destroyed);
    assert!(c.destroy(id, &auth).await.unwrap().is_confirmed());
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
}

#[tokio::test]
async fn failed_intent_write_prevents_create_and_poison_blocks_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    fs::write(
        dir.path().join("node/journal.pending"),
        b"interrupted write",
    )
    .unwrap();
    assert!(c.launch(id, &()).await.is_err());
    assert!(
        c.backend
            .list_owned(c.identity().node_id)
            .await
            .unwrap()
            .is_empty()
    );
    fs::remove_file(dir.path().join("node/journal.pending")).unwrap();
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}

#[tokio::test]
async fn unknown_inventory_retains_capacity_and_stopped_disk_is_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    c.backend.inner.inject(Fault::UnknownState);
    assert_eq!(c.inspect(id).await.unwrap().state, SandboxState::Unknown);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    let auth = DestroyAuthorization::retained(s, "retained-1").unwrap();
    assert!(c.destroy(id, &auth).await.is_err());
    c.stop(id).await.unwrap();
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes - 1
    );
    c.destroy(id, &auth).await.unwrap();
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes
    );
}

#[tokio::test]
async fn expired_permit_cannot_deliver_or_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    c.provider.expired = true;
    let id = accept(&mut c).await;
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.state().attempts[&id].phase, Phase::IssueIntent);
    assert_eq!(c.inspect(id).await.unwrap().state, SandboxState::Created);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(!dir.path().join(format!("node/permit-{id}.json")).exists());
}

#[tokio::test]
async fn destroyed_stop_proof_without_authority_poisons_and_retains_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    c.backend.inner.inject(Fault::DestroyedOnStop);
    let error = c.stop(id).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("without durable destroy authority")
    );
    assert_eq!(c.state().attempts[&id].phase, Phase::StopIntent);
    assert!(c.state().attempts[&id].stopped.is_none());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes - 1
    );
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}

#[tokio::test]
async fn destroyed_inspection_without_authority_poisons_and_retains_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    c.stop(id).await.unwrap();
    c.backend.inner.inject(Fault::DestroyedOnInspect);
    let error = c.inspect(id).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("without durable destroy authority")
    );
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes - 1
    );
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}

#[tokio::test]
async fn measured_inventory_reports_fake_statvfs_and_preserved_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    c.backend.inner.set_disk_usage(BackendDiskUsage {
        filesystem: Some(FsStats::from_blocks(4096, 1_000, 300, 250)),
        preserved_bytes: Some(777),
        retained_snapshot_bytes: None,
        saved_snapshots_over_allowance: None,
    });
    let running = c.measured_inventory().await;
    assert_eq!(running.disk_total_bytes, Some(4_096_000));
    assert_eq!(running.disk_used_bytes, Some(2_867_200));
    assert_eq!(running.disk_free_bytes, Some(1_024_000));
    assert_eq!(running.disk_reserved_bytes, Some(1));
    assert_eq!(running.preserved_count, Some(0));
    assert!(c.backend.inner.disk_queries()[0].is_empty());

    c.stop(id).await.unwrap();
    let stopped = c.measured_inventory().await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    assert_eq!(c.backend.inner.disk_queries()[1], vec![sandbox.clone()]);
    assert_eq!(stopped.preserved_count, Some(1));
    assert_eq!(stopped.preserved_bytes, Some(777));
    assert_eq!(stopped.disk_reserved_bytes, Some(1));
    // Measurement never changes the configured-capacity admission figure.
    assert_eq!(
        stopped.available_disk_bytes,
        c.inventory().available_disk_bytes
    );
    assert_eq!(stopped.retained_snapshot_bytes, None);

    let auth = DestroyAuthorization::retained(sandbox, "retained-1").unwrap();
    c.destroy(id, &auth).await.unwrap();
    let destroyed = c.measured_inventory().await;
    assert_eq!(destroyed.preserved_count, Some(0));
    assert_eq!(destroyed.disk_reserved_bytes, Some(0));
    assert!(c.backend.inner.disk_queries()[2].is_empty());
}
