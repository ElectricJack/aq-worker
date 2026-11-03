//! Controller ports of the synthetic M2/M3 invariants. Backend process counts
//! replace fixture OS children; runner transport/observation ownership stays B6.
use super::{
    recovery_tests::{Provider, open, restart, running},
    tests::{config, now_ms, request},
    *,
};
use aq_admission_sandbox::transport::{Cancellation, CancellationReason, PollPage};
use aq_session_protocol::{Digest, LaunchBinding, canonical_digest};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

fn cancellation(c: &Controller<FakeBackend, Provider>, id: Uuid) -> PollPage {
    let a = &c.state().attempts[&id];
    let mut session = a.request.manifest.bindings.session.clone();
    if let SessionIdentity::Remote { boot_id, .. } = &mut session {
        *boot_id = c.identity().boot_id;
    }
    PollPage {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.identity().clone(),
        server_time_ms: now_ms(),
        offers: vec![],
        next_after: None,
        cancellations: vec![Cancellation {
            cancellation_id: Uuid::new_v4(),
            authority_revision: 1,
            reason: CancellationReason::OwnerRequest,
            binding: LaunchBinding {
                schema_version: 1,
                session,
                offer_id: Uuid::new_v4(),
                attempt_id: id,
                assignment_generation: a.request.manifest.bindings.assignment_generation,
                execution_spec_digest: Digest::of(b"execution"),
                launch_manifest_digest: canonical_digest(&a.request.manifest).unwrap(),
                graph_access_profile_digest: Digest::of(b"graph"),
            },
        }],
    }
}
#[tokio::test]
async fn lost_acceptance_and_late_response_never_revive_expired_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    let time = Arc::new(AtomicU64::new(now_ms()));
    let clock = time.clone();
    c.clock = Box::new(move || Ok(clock.load(Ordering::SeqCst)));
    let req = request(c.identity(), 10);
    let deadline = req.hard_deadline_ms;
    let id = c.reserve(req).unwrap();
    assert!(
        c.accept_with(id, |_| async move {
            time.store(deadline, Ordering::SeqCst);
            Ok(())
        })
        .await
        .is_err()
    );
    assert_eq!(c.state().attempts[&id].phase, Phase::AcceptIntent);
    assert!(c.launch(id, &deadline).await.is_err());
    assert!(
        c.backend
            .list_owned(c.identity().node_id)
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn request_send_time_includes_network_delay_and_suspend() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let deadline = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(deadline - 1).await.unwrap();
    assert!(c.state().attempts[&id].stopped.is_none());
    c.sweep_at(deadline + 1_000_000).await.unwrap();
    assert!(c.state().attempts[&id].stopped.is_some());
    assert_ne!(c.state().attempts[&id].phase, Phase::Destroyed);
}
#[tokio::test]
async fn admission_replay_and_changed_bytes_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    let req = request(c.identity(), 10);
    let id = c.reserve(req.clone()).unwrap();
    let operation = c.state().attempts[&id].accept_operation_id;
    assert_eq!(c.reserve(req.clone()).unwrap(), id);
    let mut changed = req;
    changed.disk_bytes += 1;
    assert!(c.reserve(changed).is_err());
    assert_eq!(c.state().attempts[&id].accept_operation_id, operation);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
}
#[tokio::test]
async fn stopped_slots_do_not_bypass_pending_result_storage_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.config.recovery.max_pending_results = 1;
    let id = running(&mut c, 10).await;
    c.stop(id).await.unwrap();
    assert_eq!(c.inventory().reserved_sandboxes, 0);
    assert_eq!(
        *c.reserve(request(c.identity(), 11))
            .unwrap_err()
            .downcast_ref::<RefusalReason>()
            .unwrap(),
        RefusalReason::PendingRetention
    );
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    let auth = DestroyAuthorization::retained(s, "receipt").unwrap();
    c.destroy(id, &auth).await.unwrap();
    assert!(c.reserve(request(c.identity(), 11)).is_ok());
}
#[tokio::test]
async fn blocked_fixture_is_killed_and_reaped_on_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let page = cancellation(&c, id);
    let now = now_ms();
    c.observe_poll(&page, now).unwrap();
    let deadline = c.state().attempts[&id]
        .cancellation
        .as_ref()
        .unwrap()
        .stop_after_ms;
    c.sweep_at(deadline - 1).await.unwrap();
    assert!(c.state().attempts[&id].stopped.is_none());
    c.backend.inject(Fault::StopFailure);
    c.sweep_at(deadline).await.unwrap();
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    c.sweep_at(deadline).await.unwrap();
    let entry = c.inspect(id).await.unwrap();
    assert_eq!(entry.running_processes, Some(0));
    assert!(entry.disk_present);
    assert_eq!(c.inventory().reserved_sandboxes, 0);
}
#[tokio::test]
async fn observations_and_directive_receipts_cross_durable_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let page = cancellation(&c, id);
    c.observe_poll(&page, now_ms()).unwrap();
    let deadline = c.state().attempts[&id]
        .cancellation
        .as_ref()
        .unwrap()
        .stop_after_ms;
    c.observe_poll(&page, deadline + 100).unwrap();
    let c = restart(&dir, c);
    let observed = c.state().attempts[&id].cancellation.as_ref().unwrap();
    assert_eq!(observed.stop_after_ms, deadline);
    assert_eq!(
        observed.cancellation_id,
        page.cancellations[0].cancellation_id
    );
}
#[tokio::test]
async fn directive_receipts_are_bounded_and_exact_replay_survives_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let mut page = cancellation(&c, id);
    c.observe_poll(&page, now_ms()).unwrap();
    let first = c.state().attempts[&id].cancellation.clone().unwrap();
    for _ in 0..70 {
        page.cancellations[0].cancellation_id = Uuid::new_v4();
        c.observe_poll(&page, now_ms()).unwrap();
    }
    assert_eq!(
        c.state().attempts[&id]
            .cancellation
            .as_ref()
            .unwrap()
            .cancellation_id,
        first.cancellation_id
    );
    assert_eq!(c.state().attempts.len(), 1);
}
#[tokio::test]
async fn stopped_report_recovers_before_new_boot_registration_without_relaunch() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    c.stop(id).await.unwrap();
    c.retry_handoffs_at(1).await.unwrap();
    let mut c = restart(&dir, c);
    c.provider.retention_available = true;
    c.provider.events.clear();
    let (tx, rx) = tokio::sync::watch::channel(false);
    c.provider.stop_on_register = Some(tx);
    c.run(rx).await.unwrap();
    assert_eq!(c.provider.events, vec!["handoff", "register"]);
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
    assert!(c.launch(id, &0).await.is_err());
}
#[tokio::test]
async fn nonstart_restart_preserves_known_cancellation_and_defaults_unknown_to_expired() {
    for cancelled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = open(&dir, FakeBackend::default(), Provider::default());
        c.register().await.unwrap();
        let id = c.reserve(request(c.identity(), 10)).unwrap();
        c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
        if cancelled {
            c.observe_poll(&cancellation(&c, id), now_ms()).unwrap();
        }
        let mut c = restart(&dir, c);
        c.reconcile_at(now_ms()).await.unwrap();
        assert_eq!(c.state().attempts[&id].phase, Phase::Abandoned);
        assert_eq!(
            c.state().attempts[&id].cleanup.as_ref().unwrap().reason,
            if cancelled {
                CleanupReason::Cancellation
            } else {
                CleanupReason::NoCurrentGrant
            }
        );
        assert!(c.launch(id, &0).await.is_err());
    }
}
#[tokio::test]
async fn oversized_journal_is_refused_before_deserialization() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, FakeBackend::default(), Provider::default());
    drop(c);
    fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("node/journal.json"))
        .unwrap()
        .set_len(journal::MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(
        Controller::open(
            &dir.path().join("node"),
            Uuid::from_u128(1),
            config(),
            FakeBackend::default(),
            Provider::default()
        )
        .is_err()
    );
}
#[tokio::test]
async fn sweeps_ignore_foreign_scope_and_never_destroy_retained_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let mut foreign = c.state().attempts[&id].create.clone();
    foreign.owner.node_id = Uuid::new_v4();
    foreign = CreateRequest::new(foreign.owner, foreign.template_digest, 1, vec![]);
    let sandbox = c.backend.create(&foreign).await.unwrap();
    c.backend
        .deliver_private_material(&sandbox, PrivateMaterial::new(vec![1]))
        .await
        .unwrap();
    c.backend.start_runner(&sandbox).await.unwrap();
    c.provider.retention_available = true;
    c.sweep_at(u64::MAX).await.unwrap();
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running
    );
    assert!(c.inspect(id).await.unwrap().disk_present);
}
#[tokio::test]
async fn grace_is_durable_and_failed_destroy_is_retried_without_reextracting() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let deadline = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(deadline).await.unwrap();
    c.provider.retention_available = true;
    c.retry_handoffs_at(deadline).await.unwrap();
    assert_ne!(c.state().attempts[&id].phase, Phase::Destroyed);
    let after = c.state().attempts[&id]
        .cleanup
        .as_ref()
        .unwrap()
        .destroy_after_ms;
    let mut c = restart(&dir, c);
    c.backend.inject(Fault::DestroyFailure);
    c.retry_handoffs_at(after).await.unwrap();
    assert_ne!(c.state().attempts[&id].phase, Phase::Destroyed);
    c.retry_handoffs_at(after).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
    assert_eq!(c.provider.handoffs.len(), 1);
}
#[tokio::test]
async fn missing_inventory_never_releases_uncertain_writers() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    c.backend = FakeBackend::default();
    let mut c = restart(&dir, c);
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(c.state().attempts[&id].stopped.is_none());
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}

#[tokio::test]
async fn write_failures_never_acknowledge_unpersisted_admission() {
    for fault in [
        storage::WriteFault::BeforeWrite,
        storage::WriteFault::AfterFileSync,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = open(&dir, FakeBackend::default(), Provider::default());
        c.register().await.unwrap();
        *c.journal.storage.fault.lock().unwrap() = Some(fault);
        assert!(c.reserve(request(c.identity(), 10)).is_err());
        assert!(
            c.backend
                .list_owned(c.identity().node_id)
                .await
                .unwrap()
                .is_empty()
        );
        let c = restart(&dir, c);
        assert!(c.state().attempts.is_empty());
    }
}
#[tokio::test]
async fn uncertain_directory_sync_poison_requires_recovery_and_preserves_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    *c.journal.storage.fault.lock().unwrap() = Some(storage::WriteFault::AfterRename);
    assert!(c.reserve(request(c.identity(), 10)).is_err());
    assert!(c.reserve(request(c.identity(), 11)).is_err());
    let c = restart(&dir, c);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(c.state().attempts.len(), 1);
}
#[test]
fn recovery_configuration_is_bounded() {
    for field in 0..5 {
        let mut cfg = config();
        match field {
            0 => cfg.recovery.orphan_grace_ms = u64::MAX,
            1 => cfg.recovery.cancellation_grace_ms = 0,
            2 => cfg.recovery.sweep_interval_ms = 0,
            3 => cfg.recovery.deadline_margin_ms = 30_001,
            _ => cfg.recovery.max_pending_results = 0,
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Controller::open(
                &dir.path().join("node"),
                Uuid::from_u128(1),
                cfg,
                FakeBackend::default(),
                Provider::default()
            )
            .is_err()
        );
    }
}
#[test]
fn b5a_snapshot_remains_readable_without_recovery_fields() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, FakeBackend::default(), Provider::default());
    drop(c);
    let path = dir.path().join("node/journal.json");
    let bytes = fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["state"].get("orphans").is_none());
    assert!(value["state"].get("readoptions").is_none());
    let _ = open(&dir, FakeBackend::default(), Provider::default());
}
#[test]
fn orderly_journal_release_does_not_wait_for_an_unrelated_fork_child() {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, FakeBackend::default(), Provider::default());
    let (mut parent, child) = UnixStream::pair().unwrap();
    let pfd = parent.as_raw_fd();
    let cfd = child.as_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::close(pfd);
            let b = [1u8];
            libc::write(cfd, b.as_ptr().cast(), 1);
            let mut done = [0u8];
            libc::read(cfd, done.as_mut_ptr().cast(), 1);
            libc::_exit(0);
        }
    }
    struct Reap {
        pid: libc::pid_t,
        control: UnixStream,
    }
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.control.write_all(&[1]);
            let mut status = 0;
            unsafe {
                libc::waitpid(self.pid, &mut status, 0);
            }
        }
    }
    drop(child);
    let mut ready = [0u8];
    parent.read_exact(&mut ready).unwrap();
    let _reap = Reap {
        pid,
        control: parent,
    };
    drop(c);
    assert!(
        Controller::open(
            &dir.path().join("node"),
            Uuid::from_u128(1),
            config(),
            FakeBackend::default(),
            Provider::default()
        )
        .is_ok()
    );
}

#[tokio::test]
async fn actual_fixture_child_completes_once_and_report_retry_is_identical() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    assert_eq!(c.backend.fixture_output(&s).unwrap(), b"fixture-complete\n");
    c.stop(id).await.unwrap();
    c.retry_handoffs_at(1).await.unwrap();
    let mut c = restart(&dir, c);
    c.retry_handoffs_at(2).await.unwrap();
    assert_eq!(c.provider.handoffs, vec![s.clone(), s.clone()]);
    assert_eq!(c.backend.fixture_output(&s).unwrap(), b"fixture-complete\n");
    assert!(c.launch(id, &0).await.is_err());
    c.provider.retention_available = true;
    c.retry_handoffs_at(now_ms()).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
}
#[tokio::test]
async fn service_consumes_poll_and_enforces_cancellation_on_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    c.provider.poll = Some(cancellation(&c, id));
    c.recover_once().await.unwrap();
    let deadline = c.state().attempts[&id]
        .cancellation
        .as_ref()
        .unwrap()
        .stop_after_ms;
    assert!(c.state().attempts[&id].stopped.is_none());
    c.clock = Box::new(move || Ok(deadline));
    c.recover_once().await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
    assert_eq!(c.provider.handoffs.len(), 1);
}
#[tokio::test]
async fn unsupported_capabilities_and_forged_grants_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    let mut req = request(c.identity(), 10);
    req.required_capabilities.push(Capability::VmIsolation);
    assert!(c.reserve(req).is_err());
    let id = running(&mut c, 10).await;
    let mut permit = c.provider.permits[&id].clone();
    permit.binding.grant_generation += 1;
    assert!(c.observe_grant(&permit, now_ms()).is_err());
}
#[tokio::test]
async fn preparation_permit_does_not_rebind_execution_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    let req = request(c.identity(), 10);
    let deadline = req.hard_deadline_ms;
    let id = c.reserve(req).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    c.launch(id, &deadline).await.unwrap();
    let mut c = restart(&dir, c);
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    assert!(c.provider.requests.is_empty());
    assert!(c.state().attempts[&id].stopped.is_some());
}
