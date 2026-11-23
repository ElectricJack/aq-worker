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
include!("local_tmux/fixture.rs");

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guarded_literal_unicode_refuses_resize_and_deduplicates_input() {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    for bytes in [b"\x1b".as_slice(), b"\x1b[<43;37"] {
        assert!(
            io.submit(&Effect::Input(io_input(&lease, bytes)), clock)
                .is_err()
        );
    }
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
        terminal_io::Receipt::Resize(v) if !v.applied)
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
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "80x24");
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

// These qualifications retain their top-level names for the external --exact runners.
include!("local_tmux/viewer.rs");

include!("local_tmux/backpressure.rs");

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

include!("local_tmux/crash_stages.rs");

include!("local_tmux/start.rs");

include!("local_tmux/renewals.rs");

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

include!("local_tmux/rendered.rs");

include!("local_tmux/readers.rs");

include!("local_tmux/observed.rs");

include!("local_tmux/native_private.rs");
