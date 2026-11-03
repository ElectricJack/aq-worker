//! Reusable scenarios for C2/C7/C8. Adapters supply a dedicated test scope,
//! digest-pinned fixture image, control-channel payload, fault transport and
//! handoff verification. Assertions never depend on FakeBackend internals.
//! Use a fresh fixture per run; fixture owners must clean their dedicated scope
//! if an assertion panics. No real-provider qualification is implied by the fake.
use super::*;
use std::{collections::BTreeMap, future::Future};
use uuid::Uuid;

pub trait BackendFixture: Send {
    type Backend: PlacementBackend;
    fn backend(&mut self) -> &mut Self::Backend;
    /// A unique attempt per case, on the fixture's dedicated node scope.
    fn request(&self, case: u128) -> CreateRequest;
    fn material(&self) -> PrivateMaterial;
    /// Inject at the API/control-channel boundary, including a lost create reply.
    fn inject(&mut self, fault: Fault);
    /// Wait for and verify the fixture artifact without retaining it yet.
    fn observe_fixture(&mut self, sandbox: &SandboxIdentity) -> impl Future<Output = ()> + Send;
    /// Wait for the fixture output, verify it, copy it to retention, and return
    /// a bound receipt. Must fail if the output or durable copy is absent.
    fn verify_and_retain(
        &mut self,
        sandbox: &SandboxIdentity,
    ) -> impl Future<Output = DestroyAuthorization> + Send;
}

async fn stop_and_release<F: BackendFixture>(fixture: &mut F, sandbox: &SandboxIdentity) {
    assert!(fixture.backend().stop(sandbox).await.is_confirmed());
    let release =
        DestroyAuthorization::operator_released(sandbox.clone(), "conformance-cleanup").unwrap();
    let proof = fixture.backend().destroy(sandbox, &release).await;
    assert!(
        matches!(proof, Confirmation::Confirmed(ref p) if p.kind() == WriterProofKind::Destroyed && p.sandbox() == sandbox)
    );
}

pub async fn create_crash_before_record<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(1);
    let kind = fixture.backend().kind();
    let journal = LifecycleRecord::intent(kind, request.clone());
    fixture.inject(Fault::CrashAfterCreate);
    assert!(fixture.backend().create(&request).await.is_err());
    let owned = fixture
        .backend()
        .list_owned(request.owner.node_id)
        .await
        .unwrap();
    let entry = owned
        .iter()
        .find(|entry| entry.sandbox.owner == request.owner)
        .expect("lost create must remain discoverable");
    assert_eq!(entry.template_digest, request.template_digest);
    assert_eq!(entry.name, request.name);
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Adopt(entry.sandbox.clone())
    );
    assert_eq!(
        fixture.backend().create(&request).await.unwrap(),
        entry.sandbox
    );
    stop_and_release(fixture, &entry.sandbox).await;
}

pub async fn unknown_sandbox_in_inventory<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(2);
    let sandbox = fixture.backend().create(&request).await.unwrap();
    let owned = fixture
        .backend()
        .list_owned(request.owner.node_id)
        .await
        .unwrap();
    let entry = owned.iter().find(|entry| entry.sandbox == sandbox).unwrap();
    assert_eq!(
        reconcile(
            None,
            Some(entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Stop(sandbox.clone())
    );
    fixture.backend().stop(&sandbox).await;
    let entry = fixture.backend().inspect(&sandbox).await.unwrap();
    assert_eq!(
        reconcile(
            None,
            Some(&entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Preserve(sandbox.clone())
    );
    assert!(entry.disk_present);
    stop_and_release(fixture, &sandbox).await;
}

pub async fn destroy_failure_keeps_resources_protected<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(3);
    let sandbox = fixture.backend().create(&request).await.unwrap();
    assert!(fixture.backend().stop(&sandbox).await.is_confirmed());
    let release =
        DestroyAuthorization::operator_released(sandbox.clone(), "conformance-cleanup").unwrap();
    fixture.inject(Fault::DestroyFailure);
    assert!(matches!(
        fixture.backend().destroy(&sandbox, &release).await,
        Confirmation::Unconfirmed(_)
    ));
    assert!(
        fixture
            .backend()
            .inspect(&sandbox)
            .await
            .unwrap()
            .disk_present
    );
    stop_and_release(fixture, &sandbox).await;
}

pub async fn preserve_before_destroy_with_unretained_work<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(4);
    let journal = LifecycleRecord::intent(fixture.backend().kind(), request.clone());
    let sandbox = fixture.backend().create(&request).await.unwrap();
    let material = fixture.material();
    fixture
        .backend()
        .deliver_private_material(&sandbox, material)
        .await
        .unwrap();
    fixture.backend().start_runner(&sandbox).await.unwrap();
    fixture.observe_fixture(&sandbox).await;
    assert!(fixture.backend().stop(&sandbox).await.is_confirmed());
    let entry = fixture.backend().inspect(&sandbox).await.unwrap();
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Preserve(sandbox.clone())
    );
    assert!(entry.disk_present);
    // Handoff remains possible after stop; this proves disk preservation.
    let retained = fixture.verify_and_retain(&sandbox).await;
    assert!(
        fixture
            .backend()
            .destroy(&sandbox, &retained)
            .await
            .is_confirmed()
    );
}

pub async fn deadline_sweep_stops_rather_than_deletes<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(5);
    let journal = LifecycleRecord::intent(fixture.backend().kind(), request.clone());
    let sandbox = fixture.backend().create(&request).await.unwrap();
    let entry = fixture.backend().inspect(&sandbox).await.unwrap();
    let release =
        DestroyAuthorization::operator_released(sandbox.clone(), "conformance-cleanup").unwrap();
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&entry),
            &RetentionState::Authorized(release),
            Deadline::Expired
        ),
        ReconcileAction::Stop(sandbox.clone())
    );
    assert!(fixture.backend().stop(&sandbox).await.is_confirmed());
    assert!(
        fixture
            .backend()
            .inspect(&sandbox)
            .await
            .unwrap()
            .disk_present
    );
    stop_and_release(fixture, &sandbox).await;
}

pub async fn create_provision_run_observe_handoff_destroy<F: BackendFixture>(fixture: &mut F) {
    let request = fixture.request(6);
    let mut journal = LifecycleRecord::intent(fixture.backend().kind(), request.clone());
    let sandbox = fixture.backend().create(&request).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    let entry = fixture.backend().inspect(&sandbox).await.unwrap();
    assert_eq!(entry.template_digest, request.template_digest);
    assert_eq!(entry.sandbox.owner, request.owner);
    let material = fixture.material();
    fixture
        .backend()
        .deliver_private_material(&sandbox, material)
        .await
        .unwrap();
    fixture.backend().start_runner(&sandbox).await.unwrap();
    let retained = fixture.verify_and_retain(&sandbox).await;
    let stopped = fixture.backend().stop(&sandbox).await;
    journal.record_stopped(&stopped, false).unwrap();
    journal.record_preserved().unwrap();
    let entry = fixture.backend().inspect(&sandbox).await.unwrap();
    assert!(entry.disk_present);
    assert!(matches!(
        reconcile(
            Some(&journal),
            Some(&entry),
            &RetentionState::Authorized(retained.clone()),
            Deadline::Active
        ),
        ReconcileAction::Destroy { .. }
    ));
    let destroyed = fixture.backend().destroy(&sandbox, &retained).await;
    journal.record_destroyed(&destroyed, &retained).unwrap();
    assert!(
        !fixture
            .backend()
            .list_owned(request.owner.node_id)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.sandbox == sandbox)
    );
}

pub async fn run_all<F: BackendFixture>(fixture: &mut F) {
    create_crash_before_record(fixture).await;
    unknown_sandbox_in_inventory(fixture).await;
    destroy_failure_keeps_resources_protected(fixture).await;
    preserve_before_destroy_with_unretained_work(fixture).await;
    deadline_sweep_stops_rather_than_deletes(fixture).await;
    create_provision_run_observe_handoff_destroy(fixture).await;
}

#[derive(Default)]
pub struct FakeFixture {
    backend: FakeBackend,
    /// A separate store, deliberately outside the sandbox's disk lifetime.
    retained: BTreeMap<SandboxIdentity, Vec<u8>>,
}
impl BackendFixture for FakeFixture {
    type Backend = FakeBackend;
    fn backend(&mut self) -> &mut Self::Backend {
        &mut self.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        CreateRequest::new(
            Ownership {
                node_id: Uuid::from_u128(1),
                boot_id: Uuid::from_u128(2),
                attempt_id: Uuid::from_u128(case + 10),
            },
            TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            100,
            vec![Capability::PersistentDiskDuringAttempt],
        )
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        self.backend.inject(fault);
    }
    async fn observe_fixture(&mut self, sandbox: &SandboxIdentity) {
        assert_eq!(
            self.backend.fixture_output(sandbox).unwrap(),
            b"fixture-complete\n"
        );
    }
    async fn verify_and_retain(&mut self, sandbox: &SandboxIdentity) -> DestroyAuthorization {
        let output = self.backend.fixture_output(sandbox).unwrap();
        assert_eq!(output, b"fixture-complete\n");
        self.retained.insert(sandbox.clone(), output.to_vec());
        assert_eq!(self.retained[sandbox], b"fixture-complete\n");
        DestroyAuthorization::retained(
            sandbox.clone(),
            format!("fake-retention/{}", sandbox.backend_id),
        )
        .unwrap()
    }
}
