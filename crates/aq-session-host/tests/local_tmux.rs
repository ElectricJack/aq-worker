//! Explicit local process qualification: cargo test -p aq-session-host
//! --features test-support --test local_tmux -- --ignored
use anyhow::Result;
use aq_session_host::*;
use aq_session_protocol::{LaunchManifest, SessionIdentity, canonical_digest};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;
fn wait<T>(f: impl FnMut() -> Option<T>) -> T {
    wait_named("local process predicate", f)
}
fn wait_named<T>(label: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "{label} timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
struct Adapter {
    mode: String,
    dir: PathBuf,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: LocalCommand {
                executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
                arguments: vec![
                    "fixture".into(),
                    self.dir.to_str().unwrap().into(),
                    self.mode.clone(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, o: &HostObservation) -> Result<HarnessReadiness> {
        Ok(if o.phase == HostPhase::Stopped {
            HarnessReadiness::Exited
        } else if self.dir.join("fixture.claim").exists() {
            HarnessReadiness::Ready
        } else {
            HarnessReadiness::Unknown
        })
    }
}
struct Authority {
    root: PathBuf,
    pause: Option<String>,
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        durable_new(
            &self.root.join("caller-intent.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-guardian.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-start"),
            b"durable start authorization\n",
        )
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        durable_new(
            &self.root.join("caller-started.json"),
            &serde_json::to_vec(o)?,
        )
    }
    fn checkpoint(&mut self, s: &str) -> Result<()> {
        if self.pause.as_deref() == Some(s) {
            durable_new(&self.root.join("caller-checkpoint"), s.as_bytes())?;
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}
struct Case {
    temp: TempDir,
    manifest: LaunchManifest,
    local: LocalEnvironment,
    adapter: Adapter,
    authority: Authority,
    id: Uuid,
}
impl Case {
    fn new(mode: &str, central: bool) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("aqh-")
            .tempdir_in("/tmp")
            .unwrap();
        Self::at(temp, mode, central)
    }
    fn at(temp: TempDir, mode: &str, central: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::from_u128(2);
        let mut manifest: LaunchManifest =
            serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
        if central {
            manifest.bindings.session = SessionIdentity::Central {
                authority_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            };
        }
        let material = temp.path().join("context");
        std::fs::write(&material, b"fixture").unwrap();
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
            workspace: temp.path().canonicalize().unwrap(),
            helper_executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: "tmux 3.4".into(),
            credential_profiles: manifest
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect(),
            context_material: vec![ContextMaterial {
                artifact: manifest.context.references[0].artifact.clone(),
                path: material,
            }],
        };
        let adapter = Adapter {
            mode: mode.into(),
            dir: temp.path().join(id.to_string()),
        };
        let authority = Authority {
            root: temp.path().into(),
            pause: None,
        };
        Self {
            temp,
            manifest,
            local,
            adapter,
            authority,
            id,
        }
    }
    fn launch(&mut self, ms: u64) -> Result<SessionHandle> {
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: self.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&self.manifest)?,
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + ms,
        };
        SessionHost::launch(
            self.temp.path(),
            self.id,
            &self.manifest,
            p,
            &self.local,
            &self.adapter,
            &mut self.authority,
        )
    }
}
fn tmux(h: &SessionHandle, args: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(h.directory().join("tmux.sock"))
        .args(args)
        .output()
        .unwrap()
}
fn terminate(e: &ProcessEvidence) {
    if let Ok(fd) = ownership::verified_pidfd(e) {
        let _ = ownership::signal(&fd, libc::SIGKILL);
    }
}

fn io_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn io_setup() -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    io_setup_mode("terminal-journal")
}
fn io_setup_mode(
    mode: &str,
) -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::{Effect, Fence};
    let mut case = Case::new(mode, false);
    let host = case.launch(8000).unwrap();
    wait(|| {
        case.adapter
            .dir
            .join("fixture.claim")
            .exists()
            .then_some(())
    });
    let terminal = TerminalIdentity {
        session: case.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let io = host.bind_terminal(&terminal).unwrap();
    let now = io_now();
    let lease = ControllerLease {
        terminal: terminal.clone(),
        principal_id: Text::new("synthetic-owner").unwrap(),
        lease_id: Uuid::new_v4(),
        generation: 1,
        issued_at_ms: now,
        expires_at_ms: now + 6000,
        execution_expires_at_ms: now + 7000,
    };
    let fence = Fence {
        terminal,
        fence_id: Uuid::new_v4(),
        generation: 1,
        controller: Some(lease.clone()),
    };
    io.submit(
        &Effect::Fence(fence.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let receipt = wait(|| io.receipt(fence.fence_id).ok().flatten());
    assert!(matches!(receipt, terminal_io::Receipt::Fence(_)));
    (case, host, io, lease)
}
fn io_input(
    lease: &aq_session_protocol::ControllerLease,
    bytes: &[u8],
) -> aq_session_protocol::InputCommand {
    let now = io_now();
    aq_session_protocol::InputCommand {
        operation_id: Uuid::new_v4(),
        lease: lease.clone(),
        issued_at_ms: now,
        deadline_ms: now + 1500,
        bytes: bytes.to_vec(),
    }
}
fn io_lines(host: &SessionHandle) -> Vec<String> {
    std::fs::read_to_string(host.directory().join("fixture-input.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guarded_literal_unicode_resize_and_durable_duplicate_receipts() {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    let input = io_input(&lease, "literal; $(no-shell) 雪🙂\n".as_bytes());
    assert!(
        io.submit(&Effect::Input(input.clone()), clock)
            .unwrap()
            .is_none()
    );
    let receipt = wait(|| io.receipt(input.operation_id).ok().flatten());
    assert!(
        matches!(receipt, terminal_io::Receipt::Input(ref v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    assert_eq!(io_lines(&host), ["literal; $(no-shell) 雪🙂"]);
    let recovered = terminal_io::TerminalHandle::recover(&host, &lease.terminal).unwrap();
    assert!(
        matches!(recovered.submit(&Effect::Input(input.clone()), clock).unwrap(),
        Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::Applied)
    );
    let mut changed = input.clone();
    changed.bytes = b"different\n".to_vec();
    assert!(io.submit(&Effect::Input(changed), clock).is_err());
    let now = io_now();
    let resize = ResizeCommand {
        operation_id: Uuid::new_v4(),
        lease,
        sequence: 1,
        issued_at_ms: now,
        deadline_ms: now + 1500,
        columns: 100,
        rows: 35,
    };
    io.submit(&Effect::Resize(resize.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(resize.operation_id).ok().flatten()),
        terminal_io::Receipt::Resize(v) if v.applied)
    );
    let size = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            &io.binding().pane.pane_id,
            "#{window_width}x#{window_height}",
        ],
    );
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "100x35");
    assert_eq!(io_lines(&host).len(), 1);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn injecting_crash_before_or_after_physical_effect_is_unknown_never_replayed() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay_protocol::Effect;
    for stage in ["before-effect", "after-effect"] {
        let (_case, host, io, lease) = io_setup();
        let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
        durable_new(
            &host.directory().join(format!("terminal-fault-{stage}.arm")),
            b"synthetic fault\n",
        )
        .unwrap();
        let input = io_input(&lease, b"exactly-once-or-unknown\n");
        io.submit(&Effect::Input(input.clone()), clock).unwrap();
        wait(|| {
            host.directory()
                .join("terminal-fault.reached")
                .exists()
                .then_some(())
        });
        if stage == "after-effect" {
            wait(|| (io_lines(&host).len() == 1).then_some(()));
        }
        terminate(&host.identity().process);
        wait(|| {
            (!ownership::verified_pidfd(&host.identity().process)
                .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
            .then_some(())
        });
        let receipt = io.receipt(input.operation_id).unwrap().unwrap();
        assert!(
            matches!(receipt, terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
        );
        assert!(matches!(io.submit(&Effect::Input(input), clock).unwrap(),
            Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::DeliveryUnknown));
        assert_eq!(io_lines(&host).len(), usize::from(stage == "after-effect"));
        assert_eq!(host.observe().unwrap().phase, HostPhase::StopUncertain);
        // External fixture cleanup is not presented as a guardian stopped proof.
        terminate(&io.binding().pane.pane);
        terminate(&io.binding().pane.server);
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn encrypted_mailbox_tamper_and_missing_key_refuse_without_stopping_expiry() {
    use aq_terminal_relay_protocol::Effect;
    for missing_key in [false, true] {
        let (_case, mut host, io, lease) = io_setup();
        let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
        ownership::signal(&guardian, libc::SIGSTOP).unwrap();
        let input = io_input(&lease, b"private-input-must-not-be-in-file\n");
        io.submit(
            &Effect::Input(input.clone()),
            terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
        )
        .unwrap();
        let path = host.directory().join("terminal-effect.request");
        let encoded = std::fs::read(&path).unwrap();
        assert!(!encoded.windows(input.bytes.len()).any(|v| v == input.bytes));
        if missing_key {
            std::fs::remove_file(host.directory().join("terminal.key")).unwrap();
            assert!(terminal_io::TerminalHandle::recover(&host, &lease.terminal).is_err());
        } else {
            let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            let bytes = value["bytes"].as_array_mut().unwrap();
            let old = bytes[0].as_u64().unwrap();
            bytes[0] = serde_json::json!(old ^ 1);
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        ownership::signal(&guardian, libc::SIGCONT).unwrap();
        wait(|| {
            host.directory()
                .join("terminal.disabled")
                .exists()
                .then_some(())
        });
        assert!(io_lines(&host).is_empty());
        assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
        assert!(
            io.submit(
                &Effect::Input(io_input(&lease, b"refused\n")),
                terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap()
            )
            .is_err()
        );
        host.stop().unwrap();
        assert_eq!(
            host.wait_stopped(Duration::from_secs(5)).unwrap().phase,
            HostPhase::Stopped
        );
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn paused_injecting_deadline_and_new_controller_cannot_clear_unknown() {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::{Effect, Fence};
    let (_case, mut host, io, mut lease) = io_setup();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    durable_new(
        &host.directory().join("terminal-fault-before-effect.arm"),
        b"synthetic fault\n",
    )
    .unwrap();
    let mut input = io_input(&lease, b"must-expire-before-effect\n");
    input.deadline_ms = input.issued_at_ms + 200;
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    wait(|| {
        host.directory()
            .join("terminal-fault.reached")
            .exists()
            .then_some(())
    });
    wait(|| (io_now() >= input.deadline_ms).then_some(()));
    ownership::signal(
        &ownership::verified_pidfd(&host.identity().process).unwrap(),
        libc::SIGCONT,
    )
    .unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
    );
    lease.generation = 2;
    lease.lease_id = Uuid::new_v4();
    lease.issued_at_ms = io_now();
    lease.expires_at_ms = lease.issued_at_ms + 4000;
    let fence = Fence {
        terminal: lease.terminal.clone(),
        fence_id: Uuid::new_v4(),
        generation: 2,
        controller: Some(lease.clone()),
    };
    io.submit(&Effect::Fence(fence.clone()), clock).unwrap();
    wait(|| io.receipt(fence.fence_id).ok().flatten());
    let next = io_input(&lease, b"cannot-clear-unknown-by-takeover\n");
    io.submit(&Effect::Input(next.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(next.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
    assert!(io_lines(&host).is_empty());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn exact_observation_and_unstarted_recovery_never_launch_or_inject() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup();
    let started = std::fs::read(case.temp.path().join("caller-started.json")).unwrap();
    let observed = SessionHost::observe_existing(case.temp.path(), host.identity()).unwrap();
    assert_eq!(observed.identity(), host.identity());
    assert_eq!(
        std::fs::read(case.temp.path().join("caller-started.json")).unwrap(),
        started
    );
    let mut wrong = host.identity().clone();
    wrong.nonce = Uuid::new_v4();
    assert!(SessionHost::observe_existing(case.temp.path(), &wrong).is_err());
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let input = io_input(&lease, b"cancel-before-physical-intent\n");
    io.submit(
        &Effect::Input(input.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    assert!(
        io.reject_unstarted_input(
            input.operation_id,
            &aq_session_protocol::Digest::of(b"wrong")
        )
        .is_err()
    );
    let receipt = io
        .reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap())
        .unwrap();
    assert!(
        matches!(receipt, terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert!(!host.directory().join("terminal-effect.request").exists());
    assert!(
        matches!(io.submit(&Effect::Input(input), terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap()).unwrap(),
        Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::Rejected)
    );
    assert!(io_lines(&host).is_empty());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn tmux_pane_identifier_reuse_does_not_authorize_the_replacement_process() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup();
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let replacement = host.directory().join("synthetic-replacement");
    std::fs::create_dir(&replacement).unwrap();
    let old_pane = io.binding().pane.clone();
    // Deliberate external fixture tampering, never an exposed bridge command.
    assert!(
        tmux(
            &host,
            &[
                "respawn-pane",
                "-k",
                "-t",
                &old_pane.pane_id,
                "--",
                env!("CARGO_BIN_EXE_aq-session-host"),
                "fixture",
                replacement.to_str().unwrap(),
                "terminal-journal"
            ]
        )
        .status
        .success()
    );
    wait(|| replacement.join("fixture.claim").exists().then_some(()));
    let pid = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            &old_pane.pane_id,
            "#{pane_pid}",
        ],
    );
    assert_ne!(
        String::from_utf8(pid.stdout).unwrap().trim(),
        old_pane.pane.pid.to_string()
    );
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
    let input = io_input(&lease, b"must-not-reach-replacement\n");
    io.submit(
        &Effect::Input(input.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert_eq!(
        host.wait_stopped(Duration::from_secs(5)).unwrap().phase,
        HostPhase::Stopped
    );
    assert!(!replacement.join("fixture-input.jsonl").exists());
    assert!(
        matches!(io.reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap()).unwrap(),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn replayed_valid_encrypted_ledger_cannot_repeat_an_applied_input() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup();
    let original = std::fs::read(host.directory().join("terminal-state.json")).unwrap();
    let input = io_input(&lease, b"one-physical-effect\n");
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    // This is valid old ciphertext under the same key/AAD, not random damage.
    std::fs::write(host.directory().join("terminal-state.json"), original).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    wait(|| {
        host.directory()
            .join("terminal.disabled")
            .exists()
            .then_some(())
    });
    assert!(
        matches!(io.reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap()).unwrap(),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
    );
    assert_eq!(io_lines(&host), ["one-physical-effect"]);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn copied_launch_directory_and_old_mailbox_cannot_reset_physical_input_history() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay_protocol::Effect;
    use std::os::unix::fs::{DirBuilderExt, symlink};
    let (case, mut host, io, lease) = io_setup();
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let original_state = std::fs::read(host.directory().join("terminal-state.json")).unwrap();
    let input = io_input(&lease, b"cannot-reset-history-by-directory-copy\n");
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    let mailbox = std::fs::read(host.directory().join("terminal-effect.request")).unwrap();
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let retained = case.temp.path().join("retained-launch");
    std::fs::rename(host.directory(), &retained).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(host.directory())
        .unwrap();
    for name in [
        "launch.json",
        "identity.json",
        "state.json",
        "directory.json",
        "terminal-binding.json",
        "terminal.key",
    ] {
        std::fs::copy(retained.join(name), host.directory().join(name)).unwrap();
    }
    durable_new(
        &host.directory().join("terminal-state.json"),
        &original_state,
    )
    .unwrap();
    durable_new(&host.directory().join("terminal-effect.request"), &mailbox).unwrap();
    symlink(
        retained.join("tmux.sock"),
        host.directory().join("tmux.sock"),
    )
    .unwrap();
    assert!(io.submit(&Effect::Input(input), clock).is_err());
    assert!(terminal_io::TerminalHandle::recover(&host, &lease.terminal).is_err());
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    wait(|| {
        host.directory()
            .join("terminal.disabled")
            .exists()
            .then_some(())
    });
    assert!(!host.directory().join("fixture-input.jsonl").exists());
    let previous = std::fs::read_to_string(retained.join("fixture-input.jsonl")).unwrap();
    assert_eq!(previous.lines().count(), 1);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn read_only_output_preserves_unicode_alternate_bytes_and_does_not_resize_or_restart() {
    use aq_session_host::terminal_output::{ControlEvent, OutputObserver};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup_mode("output-pattern");
    let before = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{window_width}x#{window_height}",
        ],
    )
    .stdout;
    let mut observer = OutputObserver::open(&io).unwrap();
    wait(|| {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    assert_eq!(
        before,
        tmux(
            &host,
            &[
                "display-message",
                "-p",
                "-t",
                "aq:0.0",
                "#{window_width}x#{window_height}"
            ]
        )
        .stdout
    );
    let input = io_input(&lease, b"emit\n");
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let mut output = vec![];
    let expected = "\x1b[?1049h\x1b[2J\x1b[3;4H雪🙂e\u{301}\x1b[?1049l\r\n".as_bytes();
    wait(|| {
        for event in observer.poll().unwrap() {
            if let ControlEvent::Bytes(bytes) = event {
                output.extend(bytes);
            }
        }
        output
            .windows(expected.len())
            .any(|w| w == expected)
            .then_some(())
    });
    assert!(OutputObserver::open(&io).is_err());
    let process = observer.process().clone();
    drop(observer);
    wait(|| {
        (!ownership::verified_pidfd(&process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
        .then_some(())
    });
    assert!(OutputObserver::open(&io).is_err());
    std::fs::remove_file(host.directory().join("terminal-output.claim")).unwrap();
    assert!(
        OutputObserver::open(&io).is_err(),
        "encrypted one-shot claim survives missing marker"
    );
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    if let Some(root) = std::env::var_os("AQ_HOST_COMPAT_EVIDENCE_DIR") {
        use std::os::unix::fs::MetadataExt;
        let root = PathBuf::from(root);
        let metadata = std::fs::symlink_metadata(&root).unwrap();
        assert!(
            root.is_absolute()
                && metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o700
        );
        let running: HostObservation = serde_json::from_slice(
            &std::fs::read(case.temp.path().join("caller-started.json")).unwrap(),
        )
        .unwrap();
        let proof = host.stopped_proof().unwrap();
        assert_eq!(running.phase, HostPhase::Running);
        assert_eq!(&running.guardian, proof.guardian());
        assert_eq!(running.pane, proof.observation().pane);
        assert!(
            proof.observation().kernel_wait_echild
                && proof.observation().root_status.is_some()
                && proof.observation().reaped_count > 0
        );
        durable_new(&root.join("host-proof-pair.json"), &serde_json::to_vec_pretty(&serde_json::json!({
            "running":running,"stopped_proof":proof,"synthetic":true,"host_version":env!("CARGO_PKG_VERSION")
        })).unwrap()).unwrap();
    }
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn sole_sigstopped_observer_does_not_block_pty_progress_or_guardian_control() {
    use aq_session_host::terminal_output::{ControlEvent, OutputObserver};
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = OutputObserver::open(&io).unwrap();
    wait_named("output observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    ownership::signal(&client, libc::SIGSTOP).unwrap();
    wait_named("observer SIGSTOP state", || {
        std::fs::read_to_string(format!("/proc/{}/status", observer.process().pid))
            .ok()
            .is_some_and(|s| {
                s.lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'))
            })
            .then_some(())
    });
    let input = io_input(&lease, b"flood\n");
    let operation_id = input.operation_id;
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let stopped = Instant::now();
    let mut progress: u64;
    let progress_deadline = stopped + Duration::from_secs(5);
    loop {
        progress = std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if progress >= 4 * 1024 * 1024 && stopped.elapsed() >= Duration::from_millis(1300) {
            break;
        }
        if Instant::now() >= progress_deadline {
            eprintln!(
                "stopped-observer diagnostic receipt={:?} host={:?} root={}",
                io.receipt(operation_id),
                host.observe(),
                host.directory().display()
            );
            if let Some(pane) = host.observe().ok().and_then(|o| o.pane) {
                eprintln!(
                    "synthetic start={:?} syscall={:?}",
                    std::fs::read(host.directory().join("fixture-output-start")),
                    std::fs::read_to_string(format!("/proc/{}/syscall", pane.pane.pid))
                );
                eprintln!(
                    "synthetic pane wchan={:?} status={:?}",
                    std::fs::read_to_string(format!("/proc/{}/wchan", pane.pane.pid)),
                    std::fs::read_to_string(format!("/proc/{}/status", pane.pane.pid))
                );
            }
            std::mem::forget(_case);
            panic!(
                "stopped observer PTY progress timeout: {progress} bytes after {:?}",
                stopped.elapsed()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ownership::pidfd_is_live(&client).unwrap());
    let boot = ownership::raw_boot_ms();
    host.renew(&ExecutionPermit {
        session: lease.terminal.session.clone(),
        manifest_digest: host.identity().intent.manifest_digest.clone(),
        sequence: 2,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 6000,
    })
    .unwrap();
    wait_named("guardian applied independent renewal", || {
        (host.observe().unwrap().permit_sequence == 2).then_some(())
    });
    ownership::signal(&client, libc::SIGCONT).unwrap();
    let mut paused = false;
    wait_named("actual tmux pause-after notification", || {
        for event in observer.poll().unwrap() {
            if matches!(event, ControlEvent::Paused) {
                paused = true;
            }
        }
        paused.then_some(())
    });
    assert!(paused, "actual tmux pause-after must be observed");
    assert!(observer.poll().is_err());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    eprintln!("synthetic output progress with stopped sole observer: {progress} bytes");
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn execution_expiry_stops_writers_even_while_observer_client_remains_sigstopped() {
    use aq_session_host::terminal_output::OutputObserver;
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = OutputObserver::open(&io).unwrap();
    wait(|| {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    ownership::signal(&client, libc::SIGSTOP).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/status", observer.process().pid))
            .ok()
            .is_some_and(|s| {
                s.lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'))
            })
            .then_some(())
    });
    let boot = ownership::raw_boot_ms();
    host.renew(&ExecutionPermit {
        session: lease.terminal.session,
        manifest_digest: host.identity().intent.manifest_digest.clone(),
        sequence: 2,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 500,
    })
    .unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    assert!(
        ownership::pidfd_is_live(&client).unwrap(),
        "reader is outside the writer proof and still stopped"
    );
    let process = observer.process().clone();
    drop(observer);
    wait(|| {
        (!ownership::verified_pidfd(&process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
        .then_some(())
    });
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn central_and_remote_interaction_stop_and_no_relaunch() {
    for central in [false, true] {
        let mut c = Case::new("interactive", central);
        let mut h = c.launch(5000).unwrap();
        wait(|| std::fs::read_to_string(h.directory().join("fixture.claim")).ok());
        let o = h.observe().unwrap();
        let pane = o.pane.unwrap();
        assert_eq!(
            pane.server.pid,
            serde_json::from_slice::<ProcessEvidence>(
                &std::fs::read(h.directory().join("server.json")).unwrap()
            )
            .unwrap()
            .pid
        );
        assert!(
            tmux(&h, &["send-keys", "-l", "hello; $(unsafe)"])
                .status
                .success()
        );
        assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
        wait(|| {
            h.capture_text()
                .ok()
                .filter(|s| s.contains("echo:hello; $(unsafe)"))
        });
        h.stop().unwrap();
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        assert_eq!(h.stopped_proof().unwrap().guardian(), h.identity());
        assert!(ownership::process_evidence(pane.server.pid).is_err());
        assert!(h.capture_text().is_err());
        assert!(
            !tmux(&h, &["display-message", "-p", "#{pid}"])
                .status
                .success()
        );
        assert!(c.launch(5000).is_err());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn pane_exit_has_no_fallback_and_escaped_descendants_require_kernel_stop() {
    let mut c = Case::new("escaped", false);
    let mut h = c.launch(5000).unwrap();
    wait(|| std::fs::read(h.directory().join("escaped-process.json")).ok());
    let e: ProcessEvidence =
        serde_json::from_slice(&std::fs::read(h.directory().join("escaped-process.json")).unwrap())
            .unwrap();
    assert!(tmux(&h, &["send-keys", "-l", "exit"]).status.success());
    assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild && s.reaped_count >= 2);
    assert!(ownership::process_evidence(e.pid).is_err());
    assert_eq!(
        std::fs::read(h.directory().join("pane-exec.claim")).unwrap(),
        b"one native harness exec; no fallback shell\n"
    );
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn permit_expiry_is_independent_and_late_renewal_cannot_revive() {
    let mut c = Case::new("interactive", false);
    let mut h = c.launch(500).unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.stop_reason.as_deref().unwrap().contains("expired"));
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 1000,
    };
    assert!(h.renew(&p).is_err());
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_loss_remains_uncertain_even_after_known_processes_disappear() {
    let mut c = Case::new("interactive", true);
    let h = c.launch(5000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let o = h.observe().unwrap();
    terminate(&o.guardian.process);
    wait(|| (h.observe().unwrap().phase == HostPhase::StopUncertain).then_some(()));
    assert!(h.stop().is_err());
    let p = o.pane.unwrap();
    terminate(&p.server);
    terminate(&p.pane);
    assert_eq!(h.observe().unwrap().phase, HostPhase::StopUncertain);
}
#[test]
#[ignore = "subprocess fixture, called by local crash tests only"]
fn launch_child_fixture() {
    let Ok(root) = std::env::var("AQ_HOST_CASE_ROOT") else {
        return;
    };
    let temp = TempDir::new_in(&root).unwrap();
    let mut c = Case::at(temp, "interactive", false);
    std::fs::write(
        Path::new(&root).join("case-root"),
        c.temp.path().to_str().unwrap(),
    )
    .unwrap();
    c.authority.pause = std::env::var("AQ_HOST_CALLER_PAUSE").ok();
    let _handle = c.launch(1500);
    std::thread::sleep(Duration::from_secs(2));
    std::mem::forget(c);
}
fn spawn_case(root: &Path, stage: &str, guardian_fault: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "launch_child_fixture",
            "--test-threads=1",
        ])
        .env("AQ_HOST_CASE_ROOT", root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if guardian_fault {
        command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
    } else {
        command.env("AQ_HOST_CALLER_PAUSE", stage);
    }
    command.spawn().unwrap()
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn caller_crashes_at_durable_start_boundaries_never_relaunch() {
    for stage in [
        "launch-intent-durable",
        "guardian-spawned",
        "guardian-identity-durable",
        "start-intent-durable",
        "start-sent",
        "start-receipt-durable",
    ] {
        let root = TempDir::new().unwrap();
        let mut caller = spawn_case(root.path(), stage, false);
        let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
        let case = Path::new(&case);
        wait(|| case.join("caller-checkpoint").exists().then_some(()));
        caller.kill().unwrap();
        caller.wait().unwrap();
        let intent: LaunchIntent =
            serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap())
                .unwrap();
        if stage == "launch-intent-durable" {
            assert!(
                !case
                    .join(intent.launch_id.to_string())
                    .join("identity.json")
                    .exists()
            );
            continue;
        }
        let mut h = wait(|| SessionHost::recover(case, &intent).ok());
        let s = h
            .wait_stopped(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{stage}: {e:#}"));
        assert!(s.kernel_wait_echild);
        assert!(
            Command::new("/usr/bin/tmux")
                .args(["-N", "-S"])
                .arg(h.directory().join("tmux.sock"))
                .arg("list-sessions")
                .output()
                .is_ok_and(|o| !o.status.success())
        );
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_crashes_across_socket_and_pane_creation_never_claim_stop() {
    for stage in [
        "before-tmux-spawn",
        "after-tmux-spawn",
        "socket-created",
        "pane-created",
    ] {
        let root = TempDir::new().unwrap();
        let mut caller = spawn_case(root.path(), stage, true);
        let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
        let case = Path::new(&case);
        let dir = case.join(Uuid::from_u128(2).to_string());
        wait(|| dir.join("guardian-fault.json").exists().then_some(()));
        let identity: GuardianIdentity =
            serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
        let children: Vec<ProcessEvidence> = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            identity.process.pid, identity.process.pid
        ))
        .unwrap()
        .split_whitespace()
        .filter_map(|pid| ownership::process_evidence(pid.parse().unwrap()).ok())
        .collect();
        terminate(&identity.process);
        caller.kill().unwrap();
        caller.wait().unwrap();
        wait(|| {
            SessionHost::recover(case, &identity.intent)
                .is_err()
                .then_some(())
        });
        for child in children {
            terminate(&child);
        }
        assert!(
            !serde_json::from_slice::<HostObservation>(
                &std::fs::read(dir.join("state.json")).unwrap()
            )
            .unwrap()
            .kernel_wait_echild
        );
    }
}

struct StartGate<'a> {
    inner: &'a mut Authority,
    dir: PathBuf,
    deny: bool,
    collide: bool,
}
impl LaunchAuthority for StartGate<'_> {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        self.inner.record_launch_intent(i)
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        self.inner.record_guardian(i)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        if self.deny {
            anyhow::bail!("simulated caller fsync refusal")
        }
        if self.collide {
            durable_new(&self.dir.join("tmux.sock"), b"foreign socket sentinel")?;
        }
        self.inner.record_start_intent(i, g)
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        self.inner.record_started(o)
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn failed_durable_start_and_socket_collision_never_launch_native_child() {
    for collide in [false, true] {
        let mut c = Case::new("interactive", false);
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: c.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&c.manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + 1000,
        };
        let dir = c.temp.path().join(c.id.to_string());
        let mut gate = StartGate {
            inner: &mut c.authority,
            dir: dir.clone(),
            deny: !collide,
            collide,
        };
        assert!(
            SessionHost::launch(
                c.temp.path(),
                c.id,
                &c.manifest,
                p,
                &c.local,
                &c.adapter,
                &mut gate
            )
            .is_err()
        );
        assert!(!dir.join("fixture.claim").exists());
        assert!(!dir.join("pane-exec.claim").exists());
        let intent: LaunchIntent = serde_json::from_slice(
            &std::fs::read(c.temp.path().join("caller-intent.json")).unwrap(),
        )
        .unwrap();
        let mut h = SessionHost::recover(c.temp.path(), &intent).unwrap();
        assert!(
            h.wait_stopped(Duration::from_secs(5))
                .unwrap()
                .kernel_wait_echild
        );
        if collide {
            assert_eq!(
                std::fs::read(dir.join("tmux.sock")).unwrap(),
                b"foreign socket sentinel"
            );
        } else {
            assert!(!dir.join("start.request").exists());
        }
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn existing_pane_is_refused_and_never_used_for_the_agent() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "socket-created", true);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    let dir = case.join(Uuid::from_u128(2).to_string());
    wait(|| dir.join("guardian-fault.json").exists().then_some(()));
    let identity: GuardianIdentity =
        serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", identity.process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let status = Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(dir.join("tmux.sock"))
        .args(["new-session", "-d", "-s", "aq", "/bin/sleep", "10"])
        .status()
        .unwrap();
    assert!(status.success());
    let fd = ownership::verified_pidfd(&identity.process).unwrap();
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    let stopped = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(stopped.kernel_wait_echild);
    assert!(stopped.stop_reason.unwrap().contains("startup failed"));
    assert!(!dir.join("fixture.claim").exists());
    assert!(!dir.join("pane-exec.claim").exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn local_configuration_refusals_happen_before_durable_launch_or_process() {
    for case in ["version", "credentials", "context", "hard-resources"] {
        let mut c = Case::new("interactive", false);
        match case {
            "version" => c.local.tmux_version = "tmux unsupported".into(),
            "credentials" => c.local.credential_profiles.clear(),
            "hard-resources" => {
                c.local.resource_policy = ResourcePolicy::RequireQualifiedEnvironment(
                    c.manifest.harness.environment.clone(),
                )
            }
            "context" => std::fs::write(&c.local.context_material[0].path, b"changed").unwrap(),
            _ => unreachable!(),
        }
        assert!(c.launch(1000).is_err());
        assert!(!c.temp.path().join("caller-intent.json").exists());
        assert!(!c.temp.path().join(c.id.to_string()).exists());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewals_are_monotonic_and_resource_cost_is_observed() {
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(1000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 2000,
    };
    h.renew(&permit).unwrap();
    h.renew(&permit).unwrap();
    let mut changed = permit.clone();
    changed.expires_boot_ms += 1;
    assert!(h.renew(&changed).is_err());
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    let state = h.observe().unwrap();
    let pane = state.pane.unwrap();
    let mut rss = serde_json::Map::new();
    for (name, pid) in [
        ("guardian", state.guardian.process.pid),
        ("tmux", pane.server.pid),
        ("pane", pane.pane.pid),
    ] {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        rss.insert(name.into(), kb.into());
    }
    println!(
        "qualification_resource_rss_kib={}",
        serde_json::Value::Object(rss)
    );
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn malformed_renewal_stops_and_wall_limit_cannot_be_extended() {
    for malformed in [false, true] {
        let mut c = Case::new("interactive", false);
        if !malformed {
            c.manifest.resources.wall_time_ms = 600;
            c.manifest.resources.stop_grace_ms = 100;
        }
        let mut h = c.launch(500).unwrap();
        if malformed {
            std::fs::write(h.directory().join("renew.request"), b"{not a directive}").unwrap();
        } else {
            let now = ownership::raw_boot_ms();
            let p = ExecutionPermit {
                session: c.manifest.bindings.session.clone(),
                manifest_digest: canonical_digest(&c.manifest).unwrap(),
                sequence: 2,
                issued_boot_ms: now,
                expires_boot_ms: now + 2000,
            };
            assert!(h.renew(&p).is_err());
        }
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        if malformed {
            assert!(s.stop_reason.unwrap().contains("running validation failed"));
        } else {
            assert!(s.stop_reason.unwrap().contains("expired"));
        }
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewal_compare_publish_is_serialized_while_guardian_is_stopped() {
    use std::sync::Barrier;
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(5000).unwrap();
    let fd = ownership::verified_pidfd(&h.identity().process).unwrap();
    ownership::signal(&fd, libc::SIGSTOP).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", h.identity().process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 6000,
    };
    let mut changed = p.clone();
    changed.expires_boot_ms += 1;
    let barrier = Barrier::new(3);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            h.renew(&p)
        });
        let b = scope.spawn(|| {
            barrier.wait();
            h.renew(&changed)
        });
        barrier.wait();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.is_ok(), b.is_ok());
    let accepted = if a.is_ok() {
        p.clone()
    } else {
        changed.clone()
    };
    let rejected = if a.is_ok() { changed } else { p };
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    let mut higher = accepted.clone();
    higher.sequence = 3;
    h.renew(&higher).unwrap();
    assert!(h.renew(&accepted).is_err());
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn helper_expiry_survives_caller_death_without_recovery_control() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "start-receipt-durable", false);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    wait(|| case.join("caller-checkpoint").exists().then_some(()));
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap()).unwrap();
    caller.kill().unwrap();
    caller.wait().unwrap();
    let dir = case.join(intent.launch_id.to_string());
    let state = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(state.kernel_wait_echild);
    assert!(state.stop_reason.unwrap().contains("expired"));
    let h = SessionHost::recover(case, &intent).unwrap();
    assert_eq!(h.stopped_proof().unwrap().guardian().intent, intent);
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn sustained_adopted_child_exits_cannot_starve_expiry_or_stop_proof() {
    let mut c = Case::new("fork-flood", false);
    c.manifest.resources.stop_grace_ms = 100;
    let mut h = c.launch(700).unwrap();
    wait(|| {
        std::fs::read_to_string(h.directory().join("producer-counter"))
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 16)
    });
    let producer: ProcessEvidence = serde_json::from_slice(
        &std::fs::read(h.directory().join("producer-process.json")).unwrap(),
    )
    .unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.reaped_count >= 16);
    assert!(s.stop_reason.unwrap().contains("expired"));
    assert!(ownership::process_evidence(producer.pid).is_err());
}

fn rendered_material(c: &mut Case, bytes: &[u8]) -> RenderedStartup {
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    let digest = Digest::of(b"runtime-validated synthetic descriptor fixture");
    let descriptor = ArtifactRef {
        id: Text::new(format!("aq.startup-delivery/{}", digest.as_str())).unwrap(),
        version: Text::new("1").unwrap(),
        digest,
    };
    c.manifest.modules.push(descriptor.clone());
    c.local.context_material.clear();
    let payload = ArtifactRef {
        id: Text::new("final-startup").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    };
    RenderedStartup::new(c.manifest.clone(), descriptor, payload, bytes.to_vec()).unwrap()
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_startup_reaches_owned_harness_as_one_exact_payload() {
    let mut c = Case::new("rendered", false);
    let bytes = "framed\nfixture\n雪\0literal; $(no shell)\n".as_bytes();
    let startup = rendered_material(&mut c, bytes);
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let mut host = SessionHost::launch_rendered(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &c.adapter,
        &mut c.authority,
    )
    .unwrap();
    let received = wait(|| std::fs::read(host.directory().join("rendered-received.bin")).ok());
    assert_eq!(received, bytes);
    assert!(!host.directory().join("context-1.bin").exists());
    let configuration: serde_json::Value =
        serde_json::from_slice(&std::fs::read(host.directory().join("launch.json")).unwrap())
            .unwrap();
    assert!(configuration.get("rendered_startup").is_some());
    host.stop().unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_payload_tampered_after_intent_is_refused_before_native_exec() {
    struct Tamper {
        inner: Authority,
        path: PathBuf,
    }
    impl LaunchAuthority for Tamper {
        fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
            self.inner.record_launch_intent(i)
        }
        fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
            self.inner.record_guardian(i)
        }
        fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
            self.inner.record_start_intent(i, g)
        }
        fn record_started(&mut self, o: &HostObservation) -> Result<()> {
            self.inner.record_started(o)
        }
        fn checkpoint(&mut self, stage: &str) -> Result<()> {
            if stage == "start-intent-durable" {
                std::fs::write(&self.path, b"corrupt")?;
            }
            Ok(())
        }
    }
    let mut c = Case::new("rendered", false);
    let startup = rendered_material(&mut c, b"fixture");
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let path = c.temp.path().join(c.id.to_string());
    let mut authority = Tamper {
        inner: c.authority,
        path: path.join("context-0.bin"),
    };
    assert!(
        SessionHost::launch_rendered(
            c.temp.path(),
            c.id,
            &startup,
            permit,
            &c.local,
            &c.adapter,
            &mut authority
        )
        .is_err()
    );
    assert!(!path.join("pane-exec.claim").exists());
    assert!(!path.join("fixture.claim").exists());
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(c.temp.path().join("caller-intent.json")).unwrap())
            .unwrap();
    let mut host = SessionHost::recover(c.temp.path(), &intent).unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}
