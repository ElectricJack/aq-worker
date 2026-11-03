use super::tests::{config, now_ms, request};
use super::*;
use anyhow::Result;
use aq_admission_sandbox::{sandbox::*, transport::PollPage};
use aq_session_protocol::LaunchManifest;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Provider {
    pub events: Vec<&'static str>,
    pub stop_on_register: Option<tokio::sync::watch::Sender<bool>>,
    pub permits: BTreeMap<Uuid, DelegatedPermit>,
    pub requests: Vec<ReadoptionRequest>,
    pub retention_available: bool,
    pub handoffs: Vec<SandboxIdentity>,
    pub offline: bool,
    pub no_current_grant: bool,
    pub poll: Option<PollPage>,
    pub emitted: Vec<NodeEvent>,
    pub events_offline: bool,
}
impl PermitProvider for Provider {
    type Request = u64;
    async fn register(&mut self, r: &Registration) -> Result<()> {
        r.validate()?;
        self.events.push("register");
        if let Some(tx) = &self.stop_on_register {
            let _ = tx.send(true);
        }
        Ok(())
    }
    async fn issue(
        &mut self,
        deadline: &u64,
        s: &SandboxIdentity,
        m: &LaunchManifest,
    ) -> Result<IssuedMaterial> {
        let permit: DelegatedPermit = serde_json::from_value(serde_json::json!({
            "schema_version":1,"permit_id":Uuid::new_v4(),"credential":"opaque_secret_01234567890123456789012345",
            "binding":{"attempt_id":s.owner.attempt_id,"grant_generation":m.bindings.assignment_generation,"sandbox_id":Uuid::new_v4(),"node":{"node_id":s.owner.node_id,"boot_id":s.owner.boot_id},"channel_id":Uuid::new_v4()},
            "scope":"preparation","issued_at_ms":now_ms()-10,"expires_at_ms":deadline,"hard_deadline_ms":deadline
        }))?;
        self.permits.insert(s.owner.attempt_id, permit.clone());
        Ok(IssuedMaterial {
            permit,
            payload: PrivateMaterial::new(b"fixture".to_vec()),
        })
    }
    async fn readopt(&mut self, r: &ReadoptionRequest) -> Result<ReadoptionResponse> {
        r.validate()?;
        self.requests.push(r.clone());
        ensure!(!self.offline, "transport unavailable");
        let old: Vec<_> = r
            .sandboxes
            .iter()
            .map(|s| self.permits[&s.attempt_id].clone())
            .collect();
        let response = ReadoptionResponse {
            schema_version: 1,
            operation_id: r.operation_id,
            previous: r.previous.clone(),
            current: r.current.clone(),
            results: old
                .iter()
                .map(|p| {
                    if self.no_current_grant {
                        return ReadoptionResult::Refused {
                            sandbox_id: p.binding.sandbox_id,
                            code: ReadoptionRefusal::NoCurrentGrant,
                        };
                    }
                    let mut p = p.clone();
                    p.binding.node = r.current.clone();
                    ReadoptionResult::Rebound { permit: p }
                })
                .collect(),
        };
        response.validate_for(r, &old, now_ms())?;
        for result in &response.results {
            if let ReadoptionResult::Rebound { permit } = result {
                self.permits
                    .insert(permit.binding.attempt_id, permit.clone());
            }
        }
        Ok(response)
    }
    async fn poll(&mut self, _: &WorkerIdentity) -> Result<Option<PollPage>> {
        Ok(self.poll.take())
    }
    async fn emit(&mut self, event: &NodeEvent) -> Result<()> {
        ensure!(!self.events_offline, "node event transport unavailable");
        self.emitted.push(event.clone());
        Ok(())
    }
    async fn retain(&mut self, s: &SandboxIdentity) -> Result<Option<DestroyAuthorization>> {
        self.events.push("handoff");
        self.handoffs.push(s.clone());
        ensure!(
            self.retention_available,
            "runner extraction/handoff unavailable"
        );
        Ok(Some(DestroyAuthorization::retained(
            s.clone(),
            "central-workspace-and-evidence-receipt",
        )?))
    }
}
pub(super) fn open(
    dir: &tempfile::TempDir,
    backend: FakeBackend,
    provider: Provider,
) -> Controller<FakeBackend, Provider> {
    Controller::open(
        &dir.path().join("node"),
        Uuid::from_u128(1),
        config(),
        backend,
        provider,
    )
    .unwrap()
}
pub(super) async fn running(c: &mut Controller<FakeBackend, Provider>, n: u128) -> Uuid {
    c.register().await.unwrap();
    let req = request(c.identity(), n);
    let deadline = req.hard_deadline_ms;
    let id = c.reserve(req).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    c.launch(id, &deadline).await.unwrap();
    let mut execution = c.provider.permits[&id].clone();
    execution.scope = PermitScope::Execution;
    c.observe_grant(&execution, now_ms()).unwrap();
    c.provider.permits.insert(id, execution);
    id
}
pub(super) fn restart(
    dir: &tempfile::TempDir,
    mut c: Controller<FakeBackend, Provider>,
) -> Controller<FakeBackend, Provider> {
    let b = std::mem::take(&mut c.backend);
    let p = std::mem::take(&mut c.provider);
    drop(c);
    open(dir, b, p)
}

#[tokio::test]
async fn controller_restart_reconciles_journal_against_backend_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let boot = c.identity().boot_id;
    c.backend.inject(Fault::UnknownSandbox);
    let mut c = restart(&dir, c);
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    assert_ne!(boot, c.identity().boot_id);
    assert_eq!(c.provider.requests.len(), 1);
    let r = &c.provider.requests[0];
    assert_eq!(r.previous.boot_id, boot);
    assert_eq!(r.current, *c.identity());
    assert_eq!(r.sandboxes[0].attempt_id, id);
    assert_eq!(
        r.sandboxes[0].grant_generation,
        c.state().attempts[&id]
            .request
            .manifest
            .bindings
            .assignment_generation
    );
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running
    );
    assert!(c.launch(id, &0).await.is_err());
    assert_eq!(c.state().orphans.len(), 1);
    let orphan = c.state().orphans.values().next().unwrap();
    assert!(orphan.stopped.is_some());
    assert!(
        c.backend
            .inspect(&orphan.entry.sandbox.clone())
            .await
            .unwrap()
            .disk_present
    );
}

#[tokio::test]
async fn unretained_sandbox_is_preserved_not_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let deadline = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(deadline).await.unwrap();
    c.retry_handoffs_at(deadline).await.unwrap();
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Stopped
    );
    assert!(c.backend.inspect(&sandbox).await.unwrap().disk_present);
    assert_eq!(c.inventory().reserved_sandboxes, 0);
    assert!(c.inventory().available_disk_bytes < config().capacity.disk_bytes);
    let mut c = restart(&dir, c);
    c.provider.retention_available = true;
    c.retry_handoffs_at(deadline + c.config.recovery.orphan_grace_ms)
        .await
        .unwrap();
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Destroyed
    );
    assert_eq!(
        c.inventory().available_disk_bytes,
        config().capacity.disk_bytes
    );
}

#[tokio::test]
async fn retention_waits_for_final_writer_proof() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let deadline = c.state().attempts[&id].create.hard_deadline_ms;
    c.backend.inject(Fault::StopFailure);
    c.sweep_at(deadline).await.unwrap();
    c.provider.retention_available = true;
    c.backend.inject(Fault::StopFailure);
    c.retry_handoffs_at(deadline).await.unwrap();
    assert!(c.provider.handoffs.is_empty());
    assert!(c.state().attempts[&id].destroy_authorization.is_none());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    c.retry_handoffs_at(deadline).await.unwrap();
    assert_eq!(c.provider.handoffs.len(), 1);
    assert!(c.state().attempts[&id].stopped.is_some());
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
}

#[tokio::test]
async fn readoption_failure_reuses_operation_and_leaves_runner_until_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let deadline = c.state().attempts[&id].request.hard_deadline_ms;
    let mut c = restart(&dir, c);
    c.provider.offline = true;
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    assert_eq!(c.provider.requests[0], c.provider.requests[1]);
    c.reconcile_at(deadline).await.unwrap();
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running
    );
    assert!(c.launch(id, &deadline).await.is_err());
    c.sweep_at(deadline + c.config.recovery.deadline_margin_ms)
        .await
        .unwrap();
    assert_eq!(
        c.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Stopped
    );
}

#[tokio::test]
async fn launch_intent_crash_never_blindly_relaunches_or_releases_writers() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.register().await.unwrap();
    let req = request(c.identity(), 10);
    let deadline = req.hard_deadline_ms;
    let id = c.reserve(req).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    c.backend.inject(Fault::CrashAfterCreate);
    assert!(c.launch(id, &deadline).await.is_err());
    let mut c = restart(&dir, c);
    c.backend.inject(Fault::StopFailure);
    c.reconcile_at(now_ms()).await.unwrap();
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert!(c.state().attempts[&id].sandbox.is_some());
    assert!(c.launch(id, &deadline).await.is_err());
    c.reconcile_at(now_ms()).await.unwrap();
    assert_eq!(c.inventory().reserved_sandboxes, 0);
    assert!(c.inventory().available_disk_bytes < config().capacity.disk_bytes);
}

#[tokio::test]
async fn explicit_no_current_grant_stops_and_preserves() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    let id = running(&mut c, 10).await;
    let mut c = restart(&dir, c);
    c.provider.no_current_grant = true;
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    assert!(c.state().attempts[&id].stopped.is_some());
    assert!(c.inspect(id).await.unwrap().disk_present);
    assert_eq!(
        c.state().attempts[&id].cleanup.as_ref().unwrap().reason,
        CleanupReason::NoCurrentGrant
    );
}

#[tokio::test]
async fn unknown_orphan_is_retained_after_grace_and_operator_release_is_supported() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, FakeBackend::default(), Provider::default());
    c.backend.inject(Fault::UnknownSandbox);
    c.reconcile_at(now_ms()).await.unwrap();
    let orphan = c.state().orphans.values().next().unwrap().clone();
    assert_eq!(c.inventory().available_disk_bytes, 0);
    c.retry_handoffs_at(orphan.cleanup.destroy_after_ms)
        .await
        .unwrap();
    assert!(
        c.backend
            .inspect(&orphan.entry.sandbox)
            .await
            .unwrap()
            .disk_present
    );
    let mut c = restart(&dir, c);
    c.provider.retention_available = true;
    c.retry_handoffs_at(orphan.cleanup.destroy_after_ms)
        .await
        .unwrap();
    assert!(c.state().orphans.values().next().unwrap().destroyed);
    c.register().await.unwrap();
    c.reconcile_at(now_ms()).await.unwrap();
    let id = running(&mut c, 10).await;
    c.stop(id).await.unwrap();
    let sandbox = c.state().attempts[&id].sandbox.clone().unwrap();
    let release =
        DestroyAuthorization::operator_released(sandbox, "operator-reviewed-release").unwrap();
    c.destroy(id, &release).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
}

#[tokio::test]
async fn disk_watchdog_stops_and_preserves_only_the_attempt_over_its_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.max_concurrent_sandboxes = 2;
    let mut c = Controller::open(
        &dir.path().join("node"),
        Uuid::from_u128(1),
        cfg,
        FakeBackend::default(),
        Provider::default(),
    )
    .unwrap();
    let over = running(&mut c, 10).await;
    let under = running(&mut c, 11).await;
    let sandbox = c.state().attempts[&over].sandbox.clone().unwrap();
    let other = c.state().attempts[&under].sandbox.clone().unwrap();
    assert_eq!(
        c.state().attempts[&over]
            .create
            .resources
            .as_ref()
            .and_then(|r| r.disk_bytes),
        Some(1),
        "the reservation's disk bytes are the sandbox limit"
    );
    c.backend.set_disk_usage(&sandbox, 1);
    c.watch_disk_at(now_ms()).await.unwrap();

    let a = &c.state().attempts[&over];
    let cleanup = a.cleanup.as_ref().unwrap();
    assert_eq!(cleanup.reason, CleanupReason::SandboxDiskLimitExceeded);
    assert_eq!(
        serde_json::to_value(cleanup.reason).unwrap(),
        "sandbox_disk_limit_exceeded"
    );
    assert_eq!(a.phase, Phase::Stopped);
    assert!(a.stopped.is_some());
    let inspected = c.backend.inspect(&sandbox).await.unwrap();
    assert_eq!(inspected.state, SandboxState::Stopped);
    assert!(inspected.disk_present, "SBX5: preserved, not destroyed");

    let b = &c.state().attempts[&under];
    assert!(b.cleanup.is_none());
    assert_eq!(b.phase, Phase::Running);
    assert_eq!(
        c.backend.inspect(&other).await.unwrap().state,
        SandboxState::Running
    );
    // Without retention authorization the handoff retry never destroys it.
    c.retry_handoffs_at(now_ms()).await.unwrap();
    assert!(c.backend.inspect(&sandbox).await.unwrap().disk_present);
    assert_eq!(c.inventory().disk_limit, Some(DiskLimitMode::Watchdog));
}
