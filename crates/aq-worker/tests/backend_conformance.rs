use aq_worker::backend::*;
use uuid::Uuid;

fn request() -> CreateRequest {
    CreateRequest::new(
        Ownership {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(3),
        },
        TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
        100,
        vec![Capability::PersistentDiskDuringAttempt],
    )
}

#[tokio::test]
async fn create_crash_before_record() {
    let mut backend = FakeBackend::default();
    let request = request();
    backend.inject(Fault::CrashAfterCreate);
    assert!(backend.create(&request).await.is_err());
    let inventory = backend.list_owned(request.owner.node_id).await.unwrap();
    assert_eq!(inventory.len(), 1);
    let journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    assert!(matches!(
        reconcile(
            Some(&journal),
            Some(&inventory[0]),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Adopt(_)
    ));
    assert_eq!(
        backend.create(&request).await.unwrap(),
        inventory[0].sandbox
    );
}

#[tokio::test]
async fn unknown_sandbox_in_inventory() {
    let mut backend = FakeBackend::default();
    let request = request();
    backend.create(&request).await.unwrap();
    let inventory = backend.list_owned(request.owner.node_id).await.unwrap();
    assert!(matches!(
        reconcile(
            None,
            Some(&inventory[0]),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Stop(_)
    ));
}

#[tokio::test]
async fn destroy_failure_keeps_resources_protected() {
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request()).await.unwrap();
    let authorization =
        DestroyAuthorization::operator_released(sandbox.clone(), "release-1").unwrap();
    backend.inject(Fault::DestroyFailure);
    let result = backend.destroy(&sandbox, &authorization).await;
    assert!(matches!(result, Confirmation::Unconfirmed(_)));
    assert!(backend.inspect(&sandbox).await.unwrap().disk_present);
}

#[tokio::test]
async fn preserve_before_destroy_with_unretained_work() {
    let mut backend = FakeBackend::default();
    let request = request();
    let sandbox = backend.create(&request).await.unwrap();
    assert!(backend.stop(&sandbox).await.is_confirmed());
    let journal = LifecycleRecord::intent(BackendKind::DockerLocal, request);
    let inventory = backend.inspect(&sandbox).await.unwrap();
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&inventory),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Preserve(sandbox)
    );
    assert!(inventory.disk_present);
}

#[tokio::test]
async fn deadline_sweep_stops_rather_than_deletes() {
    let mut backend = FakeBackend::default();
    let request = request();
    let sandbox = backend.create(&request).await.unwrap();
    let inventory = backend.inspect(&sandbox).await.unwrap();
    let journal = LifecycleRecord::intent(BackendKind::DockerLocal, request);
    let retained = RetentionState::Authorized(
        DestroyAuthorization::operator_released(sandbox.clone(), "release-1").unwrap(),
    );
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&inventory),
            &retained,
            Deadline::Expired
        ),
        ReconcileAction::Stop(sandbox)
    );
}

#[tokio::test]
async fn stop_failure_and_unknown_state_do_not_prove_stopped_writers() {
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request()).await.unwrap();
    backend.inject(Fault::StopFailure);
    assert!(!backend.stop(&sandbox).await.is_confirmed());
    backend.inject(Fault::UnknownState);
    assert!(
        !backend
            .inspect(&sandbox)
            .await
            .unwrap()
            .writer_proof()
            .is_confirmed()
    );
}

#[tokio::test]
async fn create_replay_rejects_changed_digest_or_boot() {
    let mut backend = FakeBackend::default();
    let original = request();
    let sandbox = backend.create(&original).await.unwrap();
    let mut changed = original.clone();
    changed.template_digest = TemplateDigest::parse(format!("sha256:{}", "b".repeat(64))).unwrap();
    assert!(backend.create(&changed).await.is_err());
    changed = original.clone();
    changed.owner.boot_id = Uuid::from_u128(4);
    assert!(backend.create(&changed).await.is_err());
    assert_eq!(
        backend.inspect(&sandbox).await.unwrap().template_digest,
        original.template_digest
    );
}

#[tokio::test]
async fn foreign_inventory_and_wrong_retention_cannot_delete() {
    let mut backend = FakeBackend::default();
    let request = request();
    let sandbox = backend.create(&request).await.unwrap();
    backend.stop(&sandbox).await;
    let mut inventory = backend.inspect(&sandbox).await.unwrap();
    let journal = LifecycleRecord::intent(BackendKind::DockerLocal, request);
    let mut foreign = sandbox.clone();
    foreign.owner.node_id = Uuid::from_u128(99);
    let wrong = DestroyAuthorization::operator_released(foreign.clone(), "release-2").unwrap();
    assert!(!backend.destroy(&sandbox, &wrong).await.is_confirmed());
    assert!(matches!(
        reconcile(
            Some(&journal),
            Some(&inventory),
            &RetentionState::Authorized(wrong),
            Deadline::Active
        ),
        ReconcileAction::Preserve(_)
    ));
    inventory.sandbox = foreign;
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&inventory),
            &RetentionState::Unretained,
            Deadline::Expired
        ),
        ReconcileAction::IgnoreForeign
    );
    assert_eq!(
        reconcile(
            Some(&journal),
            None,
            &RetentionState::Unretained,
            Deadline::Expired
        ),
        ReconcileAction::Hold
    );
}

#[tokio::test]
async fn provision_run_handoff_and_confirmed_destroy() {
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request()).await.unwrap();
    assert!(backend.start_runner(&sandbox).await.is_err());
    let material = PrivateMaterial::new(b"fixture-private-permit".to_vec());
    assert!(!format!("{material:?}").contains("fixture-private-permit"));
    backend
        .deliver_private_material(&sandbox, material)
        .await
        .unwrap();
    backend.start_runner(&sandbox).await.unwrap();
    assert_eq!(
        backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running
    );
    assert!(backend.stop(&sandbox).await.is_confirmed());
    let inventory = backend.inspect(&sandbox).await.unwrap();
    assert!(inventory.disk_present);
    let authorization =
        DestroyAuthorization::retained(sandbox.clone(), "central-receipt-1").unwrap();
    assert!(
        backend
            .destroy(&sandbox, &authorization)
            .await
            .is_confirmed()
    );
    let inventory = backend.inspect(&sandbox).await.unwrap();
    assert_eq!(inventory.state, SandboxState::Destroyed);
    assert!(!inventory.disk_present);
    assert!(inventory.writer_proof().is_confirmed());
}

#[tokio::test]
async fn unsupported_operations_and_capabilities_refuse_explicitly() {
    let mut backend = FakeBackend::default();
    let mut request = request();
    request.required_capabilities.push(Capability::VmIsolation);
    assert!(matches!(
        backend.create(&request).await,
        Err(BackendError::UnsupportedCapability(Capability::VmIsolation))
    ));
    request.required_capabilities.pop();
    let sandbox = backend.create(&request).await.unwrap();
    assert!(matches!(
        backend.archive(&sandbox).await,
        Confirmation::Unconfirmed(UnconfirmedReason::Unsupported)
    ));
    assert!(matches!(
        backend.resume(&sandbox).await,
        Err(BackendError::UnsupportedOperation)
    ));
    assert!(matches!(
        backend.fork(&sandbox, &request).await,
        Err(BackendError::UnsupportedOperation)
    ));
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn reusable_conformance_suite_against_fake() {
    aq_worker::backend::conformance::run_all(
        &mut aq_worker::backend::conformance::FakeFixture::default(),
    )
    .await;
}

#[tokio::test]
async fn lifecycle_records_require_matching_confirmation_and_retention() {
    let request = request();
    let mut backend = FakeBackend::default();
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    let sandbox = backend.create(&request).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    assert!(journal.record_preserved().is_err());
    let authorization =
        DestroyAuthorization::retained(sandbox.clone(), "central-receipt-1").unwrap();
    assert!(
        journal
            .record_destroyed(
                &Confirmation::Unconfirmed(UnconfirmedReason::Timeout),
                &authorization
            )
            .is_err()
    );
    let proof = backend.stop(&sandbox).await;
    journal.record_stopped(&proof, false).unwrap();
    journal.record_preserved().unwrap();
    assert!(journal.record_destroyed(&proof, &authorization).is_err());
    let destroyed = backend.destroy(&sandbox, &authorization).await;
    journal
        .record_destroyed(&destroyed, &authorization)
        .unwrap();
    let roundtrip: LifecycleRecord =
        serde_json::from_str(&serde_json::to_string(&journal).unwrap()).unwrap();
    assert_eq!(roundtrip, journal);
    assert!(matches!(journal.phase, LifecyclePhase::Destroyed { .. }));
}

#[test]
fn invalid_digest_and_empty_retention_evidence_are_rejected() {
    for bad in ["latest", "sha256:abc", "", "sha256:zzzz"] {
        assert!(TemplateDigest::parse(bad).is_err());
        assert!(serde_json::from_value::<TemplateDigest>(serde_json::json!(bad)).is_err());
    }
}

#[tokio::test]
async fn stopped_status_without_process_count_is_not_proof() {
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request()).await.unwrap();
    backend.stop(&sandbox).await;
    let mut inventory = backend.inspect(&sandbox).await.unwrap();
    inventory.running_processes = None;
    assert!(!inventory.writer_proof().is_confirmed());
    inventory.running_processes = Some(1);
    assert!(!inventory.writer_proof().is_confirmed());
    assert!(DestroyAuthorization::retained(sandbox, " ").is_err());
}

#[tokio::test]
async fn inventory_faults_discover_unknown_work_and_exclude_foreign_scope() {
    let mut backend = FakeBackend::default();
    let request = request();
    backend.create(&request).await.unwrap();
    backend.inject(Fault::UnknownSandbox);
    backend.inject(Fault::ForeignSandbox);
    let inventory = backend.list_owned(request.owner.node_id).await.unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(
        inventory
            .iter()
            .all(|entry| entry.sandbox.owner.node_id == request.owner.node_id)
    );
    assert!(
        inventory
            .iter()
            .any(|entry| entry.sandbox.owner.attempt_id != request.owner.attempt_id)
    );
}

#[tokio::test]
async fn retained_running_sandbox_still_requires_stop_before_deletion() {
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request()).await.unwrap();
    backend
        .deliver_private_material(&sandbox, PrivateMaterial::new(b"permit".to_vec()))
        .await
        .unwrap();
    backend.start_runner(&sandbox).await.unwrap();
    let auth = DestroyAuthorization::retained(sandbox.clone(), "receipt").unwrap();
    assert_eq!(
        backend.destroy(&sandbox, &auth).await,
        Confirmation::Unconfirmed(UnconfirmedReason::StillRunning)
    );
    assert!(backend.inspect(&sandbox).await.unwrap().disk_present);
    assert_eq!(
        backend.fixture_output(&sandbox).unwrap(),
        b"fixture-complete\n"
    );
}

#[test]
fn backend_advertisement_strings_match_contract() {
    for (kind, expected) in [
        (BackendKind::DockerLocal, "docker-local"),
        (BackendKind::Boat, "boat"),
        (BackendKind::CloudflareContainers, "cloudflare-containers"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<BackendKind>(serde_json::json!(expected)).unwrap(),
            kind
        );
    }
    for (capability, expected) in [
        (Capability::VmIsolation, "vm-isolation"),
        (Capability::NestedDocker, "nested-docker"),
        (Capability::SuspendSnapshot, "suspend-snapshot"),
        (
            Capability::PersistentDiskDuringAttempt,
            "persistent-disk-during-attempt",
        ),
    ] {
        assert_eq!(serde_json::to_value(capability).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<Capability>(serde_json::json!(expected)).unwrap(),
            capability
        );
    }
    assert_eq!(Deadline::at(99, Some(100)), Deadline::Active);
    assert_eq!(Deadline::at(100, Some(100)), Deadline::Expired);
    assert_eq!(Deadline::at(100, None), Deadline::Unknown);
}

#[tokio::test]
async fn contradictory_destroyed_journal_never_adopts_a_live_sandbox() {
    let request = request();
    let mut backend = FakeBackend::default();
    let sandbox = backend.create(&request).await.unwrap();
    let live_inventory = backend.inspect(&sandbox).await.unwrap();
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request);
    journal.record_created(sandbox.clone()).unwrap();
    journal
        .record_stopped(&backend.stop(&sandbox).await, false)
        .unwrap();
    journal.record_preserved().unwrap();
    let auth = DestroyAuthorization::retained(sandbox.clone(), "receipt").unwrap();
    journal
        .record_destroyed(&backend.destroy(&sandbox, &auth).await, &auth)
        .unwrap();
    assert_eq!(
        reconcile(
            Some(&journal),
            Some(&live_inventory),
            &RetentionState::Authorized(auth),
            Deadline::Active
        ),
        ReconcileAction::Hold
    );
}
