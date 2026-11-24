use super::tests::{config, now_ms, request};
use super::*;
use crate::artifact_client::{
    ArtifactRoutes, ArtifactTransferStatus, BeginArtifact, CHUNK_BYTES, CommitArtifact,
};
use aq_admission_sandbox::sandbox::DelegatedPermit;
use aq_lifecycle_protocol::{
    ArtifactRole, Binding, Digest, RetentionReceipt, Text, canonical_digest, validate_retention,
};
use aq_session_protocol::LaunchManifest;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

type TestController = Controller<ObservedBackend, Provider>;

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
impl ObservedBackend {
    fn assert_intent(&self, attempt: Uuid, phase: &str) -> serde_json::Value {
        let bytes = fs::read(self.root.join("journal.json")).unwrap();
        let journal: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let row = &journal["state"]["attempts"][attempt.to_string()];
        assert_eq!(row["phase"], phase);
        row.clone()
    }

    async fn count_owned(&mut self, node: Uuid) -> usize {
        self.list_owned(node).await.unwrap().len()
    }
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
        let row = self.assert_intent(r.owner.attempt_id, "create_intent");
        let persisted: CreateRequest = serde_json::from_value(row["create"].clone()).unwrap();
        assert_eq!(persisted, *r);
        self.inner.create(r).await
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> std::result::Result<(), BackendError> {
        self.assert_intent(s.owner.attempt_id, "material_intent");
        assert_eq!(
            m.expose_for_control_channel(),
            b"fake-runner-private-payload"
        );
        self.inner.deliver_private_material(s, m).await
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> std::result::Result<(), BackendError> {
        self.assert_intent(s.owner.attempt_id, "start_intent");
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
        self.assert_intent(s.owner.attempt_id, "stop_intent");
        self.inner.stop(s).await
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        let row = self.assert_intent(s.owner.attempt_id, "destroy_intent");
        let persisted: DestroyAuthorization =
            serde_json::from_value(row["destroy_authorization"].clone()).unwrap();
        assert_eq!(persisted, *a);
        self.inner.destroy(s, a).await
    }
    async fn measured_disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        self.inner.measured_disk_usage(preserved).await
    }
}
fn open(dir: &tempfile::TempDir, bad: bool) -> TestController {
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
async fn accept(c: &mut TestController) -> Uuid {
    c.register().await.unwrap();
    let id = c.reserve(request(c.identity(), 10)).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    id
}
async fn running() -> (tempfile::TempDir, TestController, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, false);
    let id = accept(&mut c).await;
    c.launch(id, &()).await.unwrap();
    (dir, c, id)
}
#[tokio::test]
async fn journal_records_intent_before_backend_create() {
    let (dir, mut c, id) = running().await;
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
    let owned = c.backend.count_owned(c.identity().node_id).await;
    assert_eq!((c.inventory().reserved_sandboxes, owned), (1, 1));
}
async fn assert_rejected_permit(dir: &tempfile::TempDir, c: &mut TestController) {
    let id = accept(c).await;
    assert!(c.launch(id, &()).await.is_err());
    let state = c.inspect(id).await.unwrap().state;
    assert_eq!(
        (c.state().attempts[&id].phase, state),
        (Phase::IssueIntent, SandboxState::Created)
    );
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(!dir.path().join(format!("node/permit-{id}.json")).exists());
}
#[tokio::test]
async fn wrong_permit_cannot_deliver_or_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, true);
    assert_rejected_permit(&dir, &mut c).await;
}
async fn assert_stop_retry(c: &mut TestController, id: Uuid) {
    c.backend.inner.inject(Fault::StopFailure);

    for (confirmed, reserved) in [(false, 1), (true, 0)] {
        assert_eq!(c.stop(id).await.unwrap().is_confirmed(), confirmed);
        assert_eq!(c.inventory().reserved_sandboxes, reserved);
    }
}
#[tokio::test]
async fn stop_and_destroy_require_confirmation_and_retention() {
    let (_dir, mut c, id) = running().await;
    assert_stop_retry(&mut c, id).await;
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    let auth = DestroyAuthorization::retained(s, "retained-receipt-1").unwrap();
    c.backend.inner.inject(Fault::DestroyFailure);
    for confirmed in [false, true] {
        assert_eq!(
            c.destroy(id, &auth).await.unwrap().is_confirmed(),
            confirmed
        );
        assert_eq!(c.state().attempts[&id].phase == Phase::Destroyed, confirmed);
    }
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
    assert_eq!(c.backend.count_owned(c.identity().node_id).await, 0);
    fs::remove_file(dir.path().join("node/journal.pending")).unwrap();
    assert!(c.launch(id, &()).await.is_err());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}

#[tokio::test]
async fn unknown_inventory_retains_capacity_and_stopped_disk_is_reserved() {
    let (_dir, mut c, id) = running().await;
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
    assert_rejected_permit(&dir, &mut c).await;
}

fn assert_authority_error(c: &TestController, error: anyhow::Error) {
    assert!(
        error
            .to_string()
            .contains("without durable destroy authority")
    );
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes - 1
    );
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}
#[tokio::test]
async fn destroyed_stop_proof_without_authority_poisons_and_retains_disk() {
    let (_dir, mut c, id) = running().await;
    c.backend.inner.inject(Fault::DestroyedOnStop);
    let error = c.stop(id).await.unwrap_err();
    assert_authority_error(&c, error);
    assert_eq!(c.state().attempts[&id].phase, Phase::StopIntent);
    assert!(c.state().attempts[&id].stopped.is_none());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
}

#[tokio::test]
async fn destroyed_inspection_without_authority_poisons_and_retains_disk() {
    let (_dir, mut c, id) = running().await;
    c.stop(id).await.unwrap();
    c.backend.inner.inject(Fault::DestroyedOnInspect);
    let error = c.inspect(id).await.unwrap_err();
    assert_authority_error(&c, error);
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
}

fn assert_measurements(inventory: &Inventory, expected: serde_json::Value) {
    let report = serde_json::to_value(inventory).unwrap();
    for (field, value) in expected.as_object().unwrap() {
        assert_eq!(report[field], *value, "{field}");
    }
}

async fn assert_running_measurement(c: &mut TestController) {
    let running = c.measured_inventory().await;
    assert_measurements(
        &running,
        serde_json::json!({
            "disk_total_bytes": 4_096_000, "disk_used_bytes": 2_867_200,
            "disk_free_bytes": 1_024_000, "disk_reserved_bytes": 1, "preserved_count": 0
        }),
    );
    assert!(c.backend.inner.disk_queries()[0].is_empty());
}

fn assert_stopped_measurement(c: &TestController, stopped: &Inventory, sandbox: &SandboxIdentity) {
    assert_eq!(c.backend.inner.disk_queries()[1], vec![sandbox.clone()]);
    assert_measurements(
        stopped,
        serde_json::json!({
            "preserved_count": 1, "preserved_bytes": 777, "disk_reserved_bytes": 1,
            "retained_snapshot_bytes": null, "available_disk_bytes": c.inventory().available_disk_bytes,
        }),
    );
}

#[tokio::test]
async fn measured_inventory_reports_fake_statvfs_and_preserved_sizes() {
    let (_dir, mut c, id) = running().await;
    c.backend.inner.set_measured_disk_usage(BackendDiskUsage {
        filesystem: Some(FsStats::from_blocks(4096, 1_000, 300, 250)),
        preserved_bytes: Some(777),
        retained_snapshot_bytes: None,
        saved_snapshots_over_allowance: None,
    });
    assert_running_measurement(&mut c).await;
    c.stop(id).await.unwrap();
    let stopped = c.measured_inventory().await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    assert_stopped_measurement(&c, &stopped, &sandbox);
    let auth = DestroyAuthorization::retained(sandbox, "retained-1").unwrap();
    c.destroy(id, &auth).await.unwrap();
    let destroyed = c.measured_inventory().await;
    assert_eq!(destroyed.preserved_count, Some(0));
    assert_eq!(destroyed.disk_reserved_bytes, Some(0));
    assert!(c.backend.inner.disk_queries()[2].is_empty());
}

#[tokio::test]
async fn unavailable_disk_measurements_are_omitted_from_inventory() {
    let (_dir, mut c, id) = running().await;
    c.stop(id).await.unwrap();
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let auth = DestroyAuthorization::retained(sandbox, "retained-1").unwrap();
    c.destroy(id, &auth).await.unwrap();
    // Failed or unsupported measurement stays absent, including in JSON.
    c.backend
        .inner
        .set_measured_disk_usage(BackendDiskUsage::default());
    let unknown = c.measured_inventory().await;
    let report = serde_json::to_value(&unknown).unwrap();
    for field in [
        "disk_total_bytes",
        "disk_used_bytes",
        "disk_free_bytes",
        "preserved_bytes",
        "retained_snapshot_bytes",
        "saved_snapshots_over_allowance",
    ] {
        assert!(report.get(field).is_none(), "{field}");
    }
}

/// In-memory aq-server artifact routes: the server, not the worker, derives
/// the attempt binding that receipts are bound to.
struct CentralArtifacts {
    server_binding: Binding,
    state: std::sync::Mutex<(Option<BeginArtifact>, Vec<u8>, usize)>,
}
impl CentralArtifacts {
    fn new(server_binding: Binding) -> Self {
        Self {
            server_binding,
            state: Default::default(),
        }
    }
    fn uploads(&self) -> usize {
        self.state.lock().unwrap().2
    }
    fn status(&self, receipt: Option<RetentionReceipt>) -> ArtifactTransferStatus {
        let s = self.state.lock().unwrap();
        ArtifactTransferStatus {
            transfer_id: Uuid::from_u128(7),
            offset: s.1.len() as u64,
            bytes: s.0.as_ref().unwrap().bytes,
            state: if receipt.is_some() {
                "retained"
            } else {
                "uploading"
            }
            .into(),
            receipt,
        }
    }
}
impl ArtifactRoutes for CentralArtifacts {
    async fn begin(&self, r: &BeginArtifact) -> Result<ArtifactTransferStatus> {
        {
            let mut s = self.state.lock().unwrap();
            ensure!(r.attempt_id == self.server_binding.attempt_id && r.bytes > 0);
            *s = (Some(r.clone()), Vec::new(), s.2 + 1);
        }
        Ok(self.status(None))
    }
    async fn chunk(
        &self,
        _: Uuid,
        _: Uuid,
        offset: u64,
        bytes: &[u8],
    ) -> Result<ArtifactTransferStatus> {
        {
            let mut s = self.state.lock().unwrap();
            ensure!(offset == s.1.len() as u64 && bytes.len() <= CHUNK_BYTES);
            s.1.extend_from_slice(bytes);
        }
        Ok(self.status(None))
    }
    async fn commit(&self, _: Uuid, _: &CommitArtifact) -> Result<ArtifactTransferStatus> {
        let receipt = {
            let s = self.state.lock().unwrap();
            let begin = s.0.clone().unwrap();
            ensure!(Digest::of(&s.1) == begin.artifact.digest);
            RetentionReceipt {
                receipt_id: Uuid::new_v4(),
                owner: Text::new("aq-server:project")?,
                binding_digest: canonical_digest(&self.server_binding)?,
                artifact: begin.artifact,
                role: begin.role,
                bytes: begin.bytes,
                retained_at_ms: 1,
                retained_until_ms: 2,
            }
        };
        Ok(self.status(Some(receipt)))
    }
}

async fn assert_foreign_receipt_rejected(
    c: &mut TestController,
    id: Uuid,
    binding: &Binding,
    source: &[u8],
) {
    // A receipt bound to another binding never authorizes destroy.
    let foreign = CentralArtifacts::new(Binding {
        lineage_id: Uuid::new_v4(),
        ..binding.clone()
    });
    assert!(
        c.retain_source_and_destroy(id, binding, source, &foreign)
            .await
            .is_err()
    );
    assert_ne!(c.state().attempts[&id].phase, Phase::Destroyed);
    assert!(c.state().attempts[&id].destroy_authorization.is_none());
}

fn assert_source_receipt(receipts: &[RetentionReceipt], binding: &Binding, source: &[u8]) {
    validate_retention(receipts, binding).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].role, ArtifactRole::Source);
    assert_eq!(receipts[0].artifact.digest, Digest::of(source));
}

#[tokio::test]
async fn destroy_yields_receipts_that_pass_validate_retention_against_the_attempt_binding() {
    let (_dir, mut c, id) = running().await;
    assert!(c.stop(id).await.unwrap().is_confirmed());
    let owner = c.state().attempts[&id].sandbox.clone().unwrap().owner;
    let binding = Binding {
        project_id: Uuid::new_v4(),
        task_id: Uuid::new_v4(),
        lineage_id: Uuid::new_v4(),
        attempt_id: owner.attempt_id,
        attempt_generation: 1,
        session: SessionIdentity::Remote {
            node_id: owner.node_id,
            boot_id: owner.boot_id,
            attempt_id: owner.attempt_id,
            assignment_generation: 1,
            session_id: Uuid::new_v4(),
        },
    };
    // Spans two server chunks.
    let source = vec![0x5a; CHUNK_BYTES + 17];

    assert_foreign_receipt_rejected(&mut c, id, &binding, &source).await;
    let central = CentralArtifacts::new(binding.clone());
    c.backend.inner.inject(Fault::DestroyFailure);
    let first = c
        .retain_source_and_destroy(id, &binding, &source, &central)
        .await
        .unwrap();
    assert!(!first.confirmation.is_confirmed());
    // The retry reuses the journaled receipts instead of uploading again.
    let done = c
        .retain_source_and_destroy(id, &binding, &source, &central)
        .await
        .unwrap();
    assert_eq!(
        (done.confirmation.is_confirmed(), central.uploads()),
        (true, 1)
    );
    assert_eq!(done.receipts, first.receipts);
    let attempt = &c.state().attempts[&id];
    assert_eq!(attempt.phase, Phase::Destroyed);

    assert_source_receipt(&done.receipts, &binding, &source);
    assert_eq!(
        attempt
            .destroy_authorization
            .as_ref()
            .unwrap()
            .lifecycle_receipts(),
        &done.receipts[..]
    );
}
