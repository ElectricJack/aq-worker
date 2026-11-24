/// Writer stop stand-in; the guardian receipt mapping is unit-tested.
struct StoppedGuardian(WriterStop);
impl WriterGuardian for StoppedGuardian {
    fn stop_writers(&mut self) -> Result<WriterStop> {
        Ok(self.0.clone())
    }
}
/// Agent surface stand-in (no AQ or harness route exists yet).
struct FakeAgent(FakeNode);
impl AgentLifecycle for FakeAgent {
    fn request_checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<Option<RetentionReceipt>> {
        self.0.request_checkpoint(request)
    }
    fn close_input(&mut self) -> Result<InputFence> {
        self.0.close_input()
    }
    fn reconcile_effects(
        &mut self,
        fence: &InputFence,
        pending: &[Uuid],
    ) -> Result<EffectsObservation> {
        self.0.reconcile_effects(fence, pending)
    }
}

#[test]
fn suspend_wake_through_http_lifecycle_adapter() {
    let mut lifecycle = HttpLifecycle::new();
    let a = node();
    dirty_workspace(&a.layout);
    let old_secrets = plant_credentials(&a.layout);
    let parked = lifecycle.park_and_release(&a);
    drop(a);
    lifecycle.wake_successor(&parked, &old_secrets);
}

use aq_worker::backend::{self as be, PlacementBackend};
use routes::Attempt;
struct HttpLifecycle {
    shared: routes::Shared,
    base: String,
    rt: tokio::runtime::Runtime,
    sandbox: be::SandboxIdentity,
    ports: NodePorts<be::FakeBackend, StoppedGuardian, FakeAgent>,
    retention: FakeRetention,
}
impl HttpLifecycle {
    fn new() -> Self {
        let caps = caps();
        let shared: routes::Shared = Default::default();
        shared.lock().unwrap().insert(
            "predecessor-permit".into(),
            Attempt {
                binding: predecessor(),
                peer: ConformancePeer::new(predecessor(), caps.clone()).unwrap(),
                directives: Directives {
                    suspend: Some(suspend_directive()),
                    disk_release: None,
                    continuation: None,
                },
                posts: 0,
            },
        );
        let base = routes::serve(shared.clone());

        // Node A: a real placement sandbox behind the node ports.
        let mut backend = be::FakeBackend::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let request = be::CreateRequest::new(
            be::Ownership {
                node_id: Uuid::from_u128(1),
                boot_id: Uuid::from_u128(2),
                attempt_id: predecessor().attempt_id,
            },
            be::TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            100,
            vec![be::Capability::PersistentDiskDuringAttempt],
        );
        let sandbox = rt.block_on(backend.create(&request)).unwrap();
        let mut retention = FakeRetention::default();
        let agent = FakeNode::new(&mut retention);
        let ports = NodePorts::new(
            backend,
            sandbox.clone(),
            StoppedGuardian(agent.stop.clone()),
            FakeAgent(agent),
        )
        .unwrap();
        Self {
            shared,
            base,
            rt,
            sandbox,
            ports,
            retention,
        }
    }

    fn park_and_release(&mut self, a: &Node) -> Suspended {
        let mut aq_a = HttpLifecycleApi::new(&self.base, "predecessor-permit").unwrap();

        // Suspend: the directive arrives over GET, every packet goes over POST.
        let pending = aq_a.directives(predecessor().attempt_id).unwrap();
        let directive = pending.suspend.expect("suspend directive pending");
        assert_eq!(directive, suspend_directive());
        let parked = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .with_chunk_bytes(4096)
            .suspend(&directive, &mut self.ports, &mut aq_a, &mut self.retention)
            .unwrap();
        assert_eq!(
            parked.report.disposition,
            SuspensionDisposition::RetainedSuspended
        );
        assert_eq!(
            parked.receipt.disposition,
            ReceiptDisposition::ParkingRecorded
        );
        let inventory = self
            .rt
            .block_on(self.ports.backend_mut().inspect(&self.sandbox))
            .unwrap();
        assert_eq!(inventory.state, be::SandboxState::Stopped);
        assert!(inventory.disk_present, "compute release preserves disk");
        assert_eq!(self.shared.lock().unwrap()["predecessor-permit"].posts, 2);

        // A replayed directive returns the same parking receipt; AQ dedups.
        let again = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .suspend(&directive, &mut self.ports, &mut aq_a, &mut self.retention)
            .unwrap();
        assert_eq!(again.receipt, parked.receipt);

        // AQ directs disk release; the node destroys the sandbox under it.
        let release = artifact("aq-disk-release-directive");
        self.shared
            .lock()
            .unwrap()
            .get_mut("predecessor-permit")
            .unwrap()
            .directives
            .disk_release = Some(release.clone());
        let release = aq_a
            .directives(predecessor().attempt_id)
            .unwrap()
            .disk_release
            .unwrap();
        let released = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .release_disk(&release, &mut self.ports, &mut aq_a)
            .unwrap();
        assert!(released.report.protected_disk_released);
        let inventory = self
            .rt
            .block_on(self.ports.backend_mut().inspect(&self.sandbox))
            .unwrap();
        assert_eq!(inventory.state, be::SandboxState::Destroyed);
        parked
    }

    fn wake_successor(&self, parked: &Suspended, old_secrets: &[String]) {
        // Wake: AQ grants the successor a continuation attachment.
        let wake = continuation_packet(parked);
        let attachment = attachment_of(&wake);
        let mut peer_b = ConformancePeer::new(successor(), caps()).unwrap();
        peer_b.receive(&wake).unwrap();
        self.shared.lock().unwrap().insert(
            "successor-permit".into(),
            Attempt {
                binding: successor(),
                peer: peer_b,
                directives: Directives {
                    suspend: None,
                    disk_release: None,
                    continuation: Some(wake.clone()),
                },
                posts: 0,
            },
        );
        let mut aq_b = HttpLifecycleApi::new(&self.base, "successor-permit").unwrap();
        let wake = aq_b
            .directives(successor().attempt_id)
            .unwrap()
            .continuation
            .expect("continuation pending");

        let b = node();
        let mut launcher = FakeLauncher::default();
        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
        let woken = boot.wake(&wake, &self.retention, &mut launcher).unwrap();
        assert_eq!(
            std::fs::read_to_string(b.layout.workspace().join("untracked.txt")).unwrap(),
            "new work\n"
        );
        assert_eq!(launcher.launches.len(), 1);
        for path in b.layout.paths() {
            assert!(!old_secrets.contains(&std::fs::read_to_string(&path).unwrap()));
        }
        assert_eq!(woken.gate, OperationsGate::AwaitingBootstrapAck);
        let gate = boot
            .acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
            .unwrap();
        assert_eq!(gate, OperationsGate::Open);
        assert!(
            self.shared.lock().unwrap()["successor-permit"]
                .peer
                .bootstrap_acknowledged()
        );
        let reused = conflicting_checkpoint(&woken, &attachment);
        self.assert_rejected_routes(&mut aq_b, &wake, &reused);
    }

    fn assert_rejected_routes(&self, aq_b: &mut HttpLifecycleApi, wake: &Packet, reused: &Packet) {
        // Route errors surface as typed statuses, never as receipts.
        let err = aq_b.send(reused).unwrap_err();
        assert_eq!(
            err.downcast_ref::<LifecycleStatus>().map(|s| s.0.as_u16()),
            Some(409)
        );
        let mut stranger = HttpLifecycleApi::new(&self.base, "unknown-permit").unwrap();
        let err = stranger.send(wake).unwrap_err();
        assert_eq!(
            err.downcast_ref::<LifecycleStatus>().map(|s| s.0.as_u16()),
            Some(401)
        );
    }
}

fn conflicting_checkpoint(woken: &Woken, attachment: &ContinuationAttachment) -> Packet {
    Packet::seal(
        woken.ack_message_id,
        Message::Checkpoint(Checkpoint {
            binding: successor(),
            continuation: attachment.continuation.clone(),
            claim: CheckpointClaim::Missing {
                reason: Text::new("conflicting body").unwrap(),
            },
        }),
    )
    .unwrap()
}
