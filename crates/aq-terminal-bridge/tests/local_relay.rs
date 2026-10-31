//! Explicit synthetic tmux + fake-coordinator qualification, not a real grant,
//! provider adapter, renderer snapshot or product runtime capability.
#![cfg(feature = "test-support")]
use anyhow::Result;
use aq_session_host::*;
use aq_session_protocol::*;
use aq_terminal_bridge::{
    Bridge,
    http::{NodeCredential, RelayHttp},
};
use aq_terminal_relay::{
    api::{MutationReply, TerminalInfo},
    draft::ControlStatus,
};
use aq_terminal_relay_protocol::*;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Adapter {
    directory: PathBuf,
    mode: String,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, manifest: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            manifest_digest: canonical_digest(manifest)?,
            adapter: manifest.harness.adapter.clone(),
            cli_version: manifest.harness.cli_version.as_str().into(),
            environment: manifest.harness.environment.clone(),
            capabilities: manifest.harness.required_capabilities.clone(),
            resource_qualification: None,
            command: LocalCommand {
                executable: env!("CARGO_BIN_EXE_aq-terminal-bridge-test-host").into(),
                arguments: vec![
                    "fixture".into(),
                    self.directory.to_str().unwrap().into(),
                    self.mode.clone(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, observation: &HostObservation) -> Result<HarnessReadiness> {
        Ok(if observation.phase == HostPhase::Stopped {
            HarnessReadiness::Exited
        } else if self.directory.join("fixture.claim").exists() {
            HarnessReadiness::Ready
        } else {
            HarnessReadiness::Unknown
        })
    }
}
struct Authority {
    root: PathBuf,
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, v: &LaunchIntent) -> Result<()> {
        durable_new(&self.root.join("intent"), &serde_json::to_vec(v)?)
    }
    fn record_guardian(&mut self, v: &GuardianIdentity) -> Result<()> {
        durable_new(&self.root.join("guardian"), &serde_json::to_vec(v)?)
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        durable_new(&self.root.join("start"), b"durable synthetic intent\n")
    }
    fn record_started(&mut self, v: &HostObservation) -> Result<()> {
        durable_new(&self.root.join("started"), &serde_json::to_vec(v)?)
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn launch(root: &Path) -> (LaunchManifest, SessionHandle) {
    launch_mode(root, "terminal-journal")
}
fn launch_mode(root: &Path, mode: &str) -> (LaunchManifest, SessionHandle) {
    let manifest: LaunchManifest =
        serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    let context = root.join("context");
    std::fs::write(&context, b"fixture").unwrap();
    let id = Uuid::new_v4();
    let adapter = Adapter {
        directory: root.join(id.to_string()),
        mode: mode.into(),
    };
    let local = LocalEnvironment {
        resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
        workspace: root.canonicalize().unwrap(),
        helper_executable: env!("CARGO_BIN_EXE_aq-terminal-bridge-test-host").into(),
        tmux_executable: "/usr/bin/tmux".into(),
        tmux_version: "tmux 3.4".into(),
        credential_profiles: manifest
            .credentials
            .iter()
            .map(|v| v.credential_profile.clone())
            .collect(),
        context_material: vec![ContextMaterial {
            artifact: manifest.context.references[0].artifact.clone(),
            path: context,
        }],
    };
    let boot = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 10000,
    };
    let host = SessionHost::launch(
        root,
        id,
        &manifest,
        permit,
        &local,
        &adapter,
        &mut Authority { root: root.into() },
    )
    .unwrap();
    (manifest, host)
}
struct Coordinator {
    policy: TerminalPolicy,
    terminal: TerminalIdentity,
    state: Mutex<CoordinatorState>,
    stalled_upload: Option<Arc<tokio::sync::Notify>>,
}
#[derive(Default)]
struct CoordinatorState {
    registered: bool,
    stage: usize,
    fence: Option<Fence>,
    input: Option<InputCommand>,
    fence_ack: bool,
    input_uploads: usize,
    first_input_upload: Option<Vec<u8>>,
    input_acked: bool,
    resize_acked: bool,
}
fn auth(h: &HeaderMap) {
    assert_eq!(h["authorization"], "Bearer synthetic-node-key");
}
async fn register(
    State(c): State<Arc<Coordinator>>,
    h: HeaderMap,
    Json(r): Json<Register>,
) -> Json<MutationReply> {
    auth(&h);
    assert_eq!(r.terminal, c.terminal);
    assert_eq!(r.input_mode, InputMode::FiniteV1);
    c.state.lock().unwrap().registered = true;
    Json(MutationReply::Registered {
        operation_id: r.operation_id,
        info: TerminalInfo {
            terminal: r.terminal.clone(),
            session_revision: r.expected_authority_revision,
            input_mode: InputMode::FiniteV1,
            terminal_policy: c.policy.clone(),
            control: ControlStatus {
                terminal: r.terminal,
                session_revision: r.expected_authority_revision,
                generation: 0,
                lease: None,
                fence_acknowledged: false,
                input_paused: false,
            },
            last_peer_observed_at_ms: now(),
            last_output_observed_at_ms: 0,
            disconnected: true,
        },
    })
}
async fn poll(
    State(c): State<Arc<Coordinator>>,
    h: HeaderMap,
    Json(r): Json<RelayPoll>,
) -> Json<Envelope> {
    auth(&h);
    let mut state = c.state.lock().unwrap();
    assert!(state.registered);
    let effects = match state.stage {
        0 => {
            let time = now();
            let lease = ControllerLease {
                terminal: c.terminal.clone(),
                principal_id: Text::new("synthetic-owner").unwrap(),
                lease_id: Uuid::new_v4(),
                generation: 1,
                issued_at_ms: time,
                expires_at_ms: time + 6000,
                execution_expires_at_ms: time + 8000,
            };
            let fence = Fence {
                terminal: c.terminal.clone(),
                fence_id: Uuid::new_v4(),
                generation: 1,
                controller: Some(lease),
            };
            state.fence = Some(fence.clone());
            state.stage = 1;
            vec![Effect::Fence(fence)]
        }
        1 => {
            assert!(state.fence_ack);
            let time = now();
            let input = InputCommand {
                operation_id: Uuid::new_v4(),
                lease: state.fence.as_ref().unwrap().controller.clone().unwrap(),
                issued_at_ms: time,
                deadline_ms: time + 1500,
                bytes: "literal; $(no-shell) 雪🙂\n".as_bytes().to_vec(),
            };
            state.input = Some(input.clone());
            state.stage = 2;
            vec![Effect::Input(input)]
        }
        2 => {
            assert!(state.input_acked);
            let time = now();
            state.stage = 3;
            vec![Effect::Resize(ResizeCommand {
                operation_id: Uuid::new_v4(),
                lease: state.fence.as_ref().unwrap().controller.clone().unwrap(),
                sequence: 1,
                issued_at_ms: time,
                deadline_ms: time + 1500,
                columns: 110,
                rows: 33,
            })]
        }
        _ => vec![],
    };
    Json(Envelope {
        version: VERSION,
        capability: r.capability,
        peer: r.peer,
        exchange_id: Uuid::new_v4(),
        cursor: r.cursor + 1,
        effects,
    })
}
async fn upload(
    State(c): State<Arc<Coordinator>>,
    h: HeaderMap,
    Json(r): Json<PeerUpload>,
) -> (StatusCode, Json<serde_json::Value>) {
    auth(&h);
    if let Some(started) = &c.stalled_upload {
        started.notify_one();
        std::future::pending::<()>().await;
    }
    assert!(r.frames.is_empty() && r.snapshots.is_empty() && r.gaps.is_empty());
    let mut state = c.state.lock().unwrap();
    if !r.fence_acks.is_empty() {
        assert_eq!(r.fence_acks.len(), 1);
        assert_eq!(
            r.fence_acks[0].fence_id,
            state.fence.as_ref().unwrap().fence_id
        );
        state.fence_ack = true;
    }
    if !r.receipts.is_empty() {
        assert_eq!(r.receipts.len(), 1);
        assert_eq!(r.receipts[0].disposition, InputDisposition::Applied);
        assert_eq!(
            r.receipts[0].command_digest,
            canonical_digest(state.input.as_ref().unwrap()).unwrap()
        );
        state.input_uploads += 1;
        let encoded = serde_json::to_vec(&r).unwrap();
        if state.input_uploads == 1 {
            state.first_input_upload = Some(encoded);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"synthetic lost acknowledgement"})),
            );
        }
        assert_eq!(state.first_input_upload.as_ref().unwrap(), &encoded);
        state.input_acked = true;
    }
    if !r.resize_receipts.is_empty() {
        assert_eq!(r.resize_receipts.len(), 1);
        assert!(r.resize_receipts[0].applied);
        state.resize_acked = true;
    }
    (
        StatusCode::OK,
        Json(
            serde_json::to_value(UploadAck {
                version: VERSION,
                capability: r.capability,
                peer: r.peer,
                exchange_id: r.exchange_id,
                cursor: r.cursor,
                retirements: vec![],
                dropped_output: vec![],
            })
            .unwrap(),
        ),
    )
}
async fn settle(bridge: &mut Bridge) -> PeerUpload {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        bridge.recover_pending().unwrap();
        if let Some(upload) = bridge.prepare_control_upload().unwrap() {
            return upload;
        }
        assert!(Instant::now() < deadline, "synthetic relay receipt timeout");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
#[ignore = "explicit local tmux 3.4 synthetic relay qualification"]
async fn actual_guardian_http_relay_lost_ack_restart_unicode_resize_no_duplicate() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (manifest, mut host) = launch(temp.path());
    let terminal = TerminalIdentity {
        session: manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = terminal.session
    else {
        panic!("remote fixture");
    };
    let peer = Peer::Remote { node_id, boot_id };
    let coordinator = Arc::new(Coordinator {
        terminal: terminal.clone(),
        policy: manifest.terminal.clone(),
        state: Mutex::new(CoordinatorState::default()),
        stalled_upload: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/terminal-relay/register", post(register))
        .route("/v1/terminal-relay/poll", post(poll))
        .route("/v1/terminal-relay/control-upload", post(upload))
        .with_state(coordinator.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http = RelayHttp::development_loopback(
        &origin,
        &NodeCredential::new("synthetic-node-key".into()).unwrap(),
    )
    .unwrap();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    let mut bridge = Bridge::create(&temp.path().join("relay"), peer.clone(), clock).unwrap();
    bridge
        .attach(Arc::new(host.bind_terminal(&terminal).unwrap()))
        .unwrap();
    let registration = bridge.prepare_registration(&terminal, 1).unwrap();
    http.register(&registration, &manifest.terminal)
        .await
        .unwrap();
    bridge.confirm_registration(&registration).unwrap();
    let envelope = http.poll(&bridge.poll_request(0).unwrap()).await.unwrap();
    for effect in bridge.receive(&envelope).unwrap() {
        bridge.execute(effect).unwrap();
    }
    let receipt = settle(&mut bridge).await;
    let ack = http.control_upload(&receipt).await.unwrap();
    bridge.acknowledge_control_upload(&ack).unwrap();
    let envelope = http.poll(&bridge.poll_request(0).unwrap()).await.unwrap();
    for effect in bridge.receive(&envelope).unwrap() {
        bridge.execute(effect).unwrap();
    }
    let receipt = settle(&mut bridge).await;
    assert!(http.control_upload(&receipt).await.is_err());
    assert_eq!(coordinator.state.lock().unwrap().input_uploads, 1);
    drop(bridge);
    let observed = SessionHost::observe_existing(temp.path(), host.identity()).unwrap();
    let mut bridge = Bridge::open(&temp.path().join("relay"), peer, clock).unwrap();
    bridge
        .attach(Arc::new(
            terminal_io::TerminalHandle::recover(&observed, &terminal).unwrap(),
        ))
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&bridge.prepare_registration(&terminal, 1).unwrap()).unwrap(),
        serde_json::to_vec(&registration).unwrap()
    );
    let retry = bridge.prepare_control_upload().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&retry).unwrap(),
        serde_json::to_vec(&receipt).unwrap()
    );
    let ack = http.control_upload(&retry).await.unwrap();
    bridge.acknowledge_control_upload(&ack).unwrap();
    let envelope = http.poll(&bridge.poll_request(0).unwrap()).await.unwrap();
    for effect in bridge.receive(&envelope).unwrap() {
        bridge.execute(effect).unwrap();
    }
    let receipt = settle(&mut bridge).await;
    let ack = http.control_upload(&receipt).await.unwrap();
    bridge.acknowledge_control_upload(&ack).unwrap();
    assert!(coordinator.state.lock().unwrap().resize_acked);
    let lines: Vec<String> = std::fs::read_to_string(host.directory().join("fixture-input.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines, ["literal; $(no-shell) 雪🙂"]);
    let size = std::process::Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(host.directory().join("tmux.sock"))
        .args([
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{window_width}x#{window_height}",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "110x33");
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    server.abort();
}

#[tokio::test]
#[ignore = "explicit local tmux 3.4 synthetic relay qualification"]
async fn stalled_receipt_network_does_not_hold_guardian_renewal_or_stop() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (manifest, mut host) = launch(temp.path());
    let terminal = TerminalIdentity {
        session: manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = terminal.session
    else {
        panic!("remote fixture");
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let coordinator = Arc::new(Coordinator {
        terminal: terminal.clone(),
        policy: manifest.terminal.clone(),
        state: Mutex::new(CoordinatorState::default()),
        stalled_upload: Some(started.clone()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/terminal-relay/register", post(register))
        .route("/v1/terminal-relay/poll", post(poll))
        .route("/v1/terminal-relay/control-upload", post(upload))
        .with_state(coordinator);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http = RelayHttp::development_loopback(
        &origin,
        &NodeCredential::new("synthetic-node-key".into()).unwrap(),
    )
    .unwrap();
    let mut bridge = Bridge::create(
        &temp.path().join("relay"),
        Peer::Remote { node_id, boot_id },
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    bridge
        .attach(Arc::new(host.bind_terminal(&terminal).unwrap()))
        .unwrap();
    let registration = bridge.prepare_registration(&terminal, 1).unwrap();
    http.register(&registration, &manifest.terminal)
        .await
        .unwrap();
    bridge.confirm_registration(&registration).unwrap();
    let envelope = http.poll(&bridge.poll_request(0).unwrap()).await.unwrap();
    for effect in bridge.receive(&envelope).unwrap() {
        bridge.execute(effect).unwrap();
    }
    let receipt = settle(&mut bridge).await;
    let request = receipt.clone();
    let task_http = http.clone();
    let network = tokio::spawn(async move { task_http.control_upload(&request).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    // Journal observation remains available; no lock crosses the HTTP await.
    assert_eq!(
        bridge
            .prepare_control_upload()
            .unwrap()
            .unwrap()
            .exchange_id,
        receipt.exchange_id
    );
    let boot = ownership::raw_boot_ms();
    host.renew(&ExecutionPermit {
        session: manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 9000,
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while host.observe().unwrap().permit_sequence != 2 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(!network.is_finished());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert!(!network.is_finished());
    network.abort();
    server.abort();
}

#[tokio::test]
#[ignore = "explicit local tmux 3.4 bounded tail qualification"]
async fn full_tail_queue_keeps_draining_without_a_network_consumer() {
    use aq_session_host::terminal_output::OutputObserver;
    use aq_terminal_bridge::output::{OutputTail, TailState};
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (manifest, mut host) = launch_mode(temp.path(), "output-flood");
    let terminal = TerminalIdentity {
        session: manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let io = host.bind_terminal(&terminal).unwrap();
    let mut observer = OutputObserver::open(&io).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !observer.is_attached() {
        observer.poll().unwrap();
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let time = now();
    let lease = ControllerLease {
        terminal: terminal.clone(),
        principal_id: Text::new("synthetic-owner").unwrap(),
        lease_id: Uuid::new_v4(),
        generation: 1,
        issued_at_ms: time,
        expires_at_ms: time + 6000,
        execution_expires_at_ms: time + 8000,
    };
    let fence = Fence {
        terminal: terminal.clone(),
        fence_id: Uuid::new_v4(),
        generation: 1,
        controller: Some(lease.clone()),
    };
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    io.submit(&Effect::Fence(fence.clone()), clock).unwrap();
    while io.receipt(fence.fence_id).ok().flatten().is_none() {
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(Instant::now() < deadline);
    }
    let mut policy = manifest.terminal;
    policy.max_frame_bytes = 1024;
    policy.replay_bytes = 8192;
    let mut tail = OutputTail::new(terminal, &policy).unwrap();
    let time = now();
    io.submit(
        &Effect::Input(InputCommand {
            operation_id: Uuid::new_v4(),
            lease,
            issued_at_ms: time,
            deadline_ms: time + 1500,
            bytes: b"flood\n".to_vec(),
        }),
        clock,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut drained = 0usize;
    while drained < 256 * 1024 {
        for event in observer.poll().unwrap() {
            if let aq_session_host::terminal_output::ControlEvent::Bytes(bytes) = &event {
                drained += bytes.len();
            }
            tail.accept(&event).unwrap();
        }
        assert!(tail.retained_bytes() <= 8192);
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(tail.gaps().next().is_some());
    assert_eq!(tail.state(), TailState::InitialIncomplete);
    assert!(tail.frames().all(|f| f.terminal.epoch == 1));
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}
