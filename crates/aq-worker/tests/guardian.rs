#![cfg(feature = "test-support")]
use aq_worker::{
    guardian::{GuardianIdentity, GuardianPhase, GuardianState},
    journal::{ProcessEvidence, State},
};
use aq_worker_protocol::{Phase, ProcessOutcome, StopCertainty};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output},
    time::{Duration, Instant},
};
use tempfile::TempDir;
fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_aq-worker-test-host"));
    c.arg("--state-dir")
        .arg(root)
        .arg("--fixture-agent")
        .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"));
    c
}
fn wait<T>(mut predicate: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        assert!(Instant::now() < end, "process predicate timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn state(root: &Path) -> State {
    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap()
}
fn directory(root: &Path) -> std::path::PathBuf {
    root.join("launches").join(
        state(root)
            .attempts
            .values()
            .next()
            .unwrap()
            .launch_id
            .unwrap()
            .to_string(),
    )
}
fn start(root: &Path, checkpoint: &str, descendants: bool) -> Child {
    let mut c = command(root);
    c.args(["--fault-checkpoint", checkpoint, "--fault-action", "pause"])
        .env("AQ_FIXTURE_HOLD", "1");
    if descendants {
        c.env("AQ_FIXTURE_DESCENDANT", "double-fork-setsid")
            .env("AQ_FIXTURE_IGNORE_TERM", "1");
    }
    c.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
fn marker(root: &Path, name: &str) {
    wait(|| {
        root.join("faults")
            .join(format!("{name}.json"))
            .exists()
            .then_some(())
    });
}
fn kill(child: &mut Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}
fn recover(root: &Path) -> Output {
    command(root)
        .args(["--action", "recover"])
        .output()
        .unwrap()
}
fn count(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
fn stop_test_process(evidence: &ProcessEvidence) {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, evidence.pid, 0) } as i32;
    if fd < 0 {
        return;
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    if aq_worker::fixture::process_evidence(evidence.pid)
        .ok()
        .as_ref()
        == Some(evidence)
    {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
    }
}
#[test]
fn spawn_boundary_crashes_never_duplicate_and_recover_owned_writers() {
    for checkpoint in [
        "grant-durable",
        "launch-intent-durable",
        "guardian-spawned",
        "guardian-identity-durable",
        "start-intent-durable",
        "start-sent-before-receipt",
        "start-receipt-durable",
    ] {
        let temp = TempDir::new().unwrap();
        let mut child = start(temp.path(), checkpoint, true);
        marker(temp.path(), checkpoint);
        if matches!(
            checkpoint,
            "start-sent-before-receipt" | "start-receipt-durable"
        ) {
            let dir = directory(temp.path());
            wait(|| count(&dir.join("descendant-counter")).filter(|n| *n >= 2));
        }
        kill(&mut child);
        let result = recover(temp.path());
        assert!(
            result.status.success(),
            "{checkpoint}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let state = state(temp.path());
        assert_eq!(state.attempts.len(), 1);
        let record = state.attempts.values().next().unwrap();
        assert_eq!(record.phase, Phase::Reporting);
        assert!(matches!(
            record.stop_certainty,
            StopCertainty::NotStarted | StopCertainty::ConfirmedStopped
        ));
        if record.start_intent {
            let proof = record.stopped_proof.as_ref().unwrap();
            assert!(proof.kernel_wait_echild);
            if matches!(
                checkpoint,
                "start-sent-before-receipt" | "start-receipt-durable"
            ) {
                assert!(proof.reaped_count >= 2);
            }
        }
        let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(output["protected_usage"]["slots"], 0);
        assert_eq!(output["available"]["slots"], 1);
    }
}
#[test]
fn surviving_double_fork_setsid_writer_stops_by_live_guardian_and_not_pid_scan() {
    let temp = TempDir::new().unwrap();
    let mut child = start(temp.path(), "start-receipt-durable", true);
    marker(temp.path(), "start-receipt-durable");
    let dir = directory(temp.path());
    let before = wait(|| count(&dir.join("descendant-counter")).filter(|n| *n >= 2));
    kill(&mut child);
    wait(|| count(&dir.join("descendant-counter")).filter(|n| *n > before));
    let result = recover(temp.path());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let proof = record.stopped_proof.unwrap();
    assert_eq!(proof.phase, GuardianPhase::Stopped);
    assert!(proof.kernel_wait_echild && proof.reaped_count >= 2);
    assert_eq!(record.report.unwrap().outcome, ProcessOutcome::Cancelled);
}
#[test]
fn guardian_loss_holds_resources_even_after_known_pids_disappear() {
    let temp = TempDir::new().unwrap();
    let mut child = start(temp.path(), "start-receipt-durable", true);
    marker(temp.path(), "start-receipt-durable");
    let dir = directory(temp.path());
    wait(|| count(&dir.join("descendant-counter")).filter(|n| *n >= 2));
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let descendant: ProcessEvidence =
        serde_json::from_slice(&fs::read(dir.join("descendant.json")).unwrap()).unwrap();
    kill(&mut child);
    stop_test_process(&record.guardian.as_ref().unwrap().process);
    let result = recover(temp.path());
    assert_eq!(result.status.code(), Some(2));
    let held: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(held["protected_usage"]["slots"], 1);
    assert_eq!(held["available"]["slots"], 0);
    assert_eq!(
        state(temp.path())
            .attempts
            .values()
            .next()
            .unwrap()
            .stop_certainty,
        StopCertainty::StopUncertain
    );
    stop_test_process(record.process.as_ref().unwrap());
    stop_test_process(&descendant);
    wait(|| {
        (aq_worker::fixture::process_evidence(record.process.as_ref().unwrap().pid).is_err()
            && aq_worker::fixture::process_evidence(descendant.pid).is_err())
        .then_some(())
    });
    let again = recover(temp.path());
    assert_eq!(again.status.code(), Some(2));
    assert_eq!(
        state(temp.path())
            .attempts
            .values()
            .next()
            .unwrap()
            .stop_certainty,
        StopCertainty::StopUncertain
    );
}
#[test]
fn guardian_independently_expires_while_runtime_is_dead() {
    let temp = TempDir::new().unwrap();
    let offset = temp.path().join("offset");
    fs::write(&offset, "0").unwrap();
    let mut c = command(temp.path());
    c.args([
        "--fault-checkpoint",
        "start-receipt-durable",
        "--fault-action",
        "pause",
    ])
    .env("AQ_FIXTURE_HOLD", "1")
    .env("AQ_FIXTURE_DESCENDANT", "double-fork-setsid")
    .env("AQ_FIXTURE_IGNORE_TERM", "1")
    .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset);
    let mut child = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    marker(temp.path(), "start-receipt-durable");
    let dir = directory(temp.path());
    wait(|| count(&dir.join("descendant-counter")));
    kill(&mut child);
    aq_worker::guardian::durable_replace(&offset, b"100000").unwrap();
    let proof: GuardianState = wait(|| {
        serde_json::from_slice::<GuardianState>(&fs::read(dir.join("guardian-state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == GuardianPhase::Stopped)
    });
    assert_eq!(proof.stop_reason, Some(ProcessOutcome::Expired));
    assert!(proof.kernel_wait_echild && proof.reaped_count >= 2);
    let result = recover(temp.path());
    assert!(result.status.success());
}
#[test]
fn clock_failure_uses_raw_clock_to_escalate_ignored_term() {
    for unreadable in [false, true] {
        let temp = TempDir::new().unwrap();
        let offset = temp.path().join("offset");
        fs::write(&offset, "0").unwrap();
        let mut c = command(temp.path());
        c.args([
            "--fault-checkpoint",
            "start-receipt-durable",
            "--fault-action",
            "pause",
        ])
        .env("AQ_FIXTURE_HOLD", "1")
        .env("AQ_FIXTURE_IGNORE_TERM", "1")
        .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset);
        let mut child = c
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        marker(temp.path(), "start-receipt-durable");
        let dir = directory(temp.path());
        kill(&mut child);
        if unreadable {
            fs::remove_file(&offset).unwrap();
        } else {
            aq_worker::guardian::durable_replace(&offset, b"1000").unwrap();
            let identity: GuardianIdentity =
                serde_json::from_slice(&fs::read(dir.join("guardian-identity.json")).unwrap())
                    .unwrap();
            let observed = aq_worker::guardian::control(
                &dir,
                &identity,
                aq_worker::guardian::Operation::Status,
                || Ok(()),
            )
            .unwrap();
            assert!(observed.observed_at_ms >= 1000);
            aq_worker::guardian::durable_replace(&offset, b"0").unwrap();
        }
        let proof: GuardianState = wait(|| {
            serde_json::from_slice::<GuardianState>(
                &fs::read(dir.join("guardian-state.json")).ok()?,
            )
            .ok()
            .filter(|s| s.phase == GuardianPhase::Stopped)
        });
        assert_eq!(proof.stop_reason, Some(ProcessOutcome::Expired));
        assert!(proof.kernel_wait_echild);
        assert_eq!(
            proof.root_status.map(|s| libc::WTERMSIG(s)),
            Some(libc::SIGKILL)
        );
    }
}
#[test]
fn exited_root_is_not_stop_proof_for_escaped_descendant() {
    let temp = TempDir::new().unwrap();
    let result = command(temp.path())
        .env("AQ_FIXTURE_DESCENDANT", "double-fork-setsid-root-exit")
        .env("AQ_FIXTURE_IGNORE_TERM", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let proof = record.stopped_proof.unwrap();
    assert!(proof.kernel_wait_echild && proof.reaped_count >= 2);
    assert_eq!(proof.root_status, Some(0));
    assert_eq!(record.report.unwrap().outcome, ProcessOutcome::Failed);
}
#[test]
fn guardian_does_not_inherit_runtime_lock_or_publish_token() {
    let temp = TempDir::new().unwrap();
    let mut child = start(temp.path(), "start-receipt-durable", false);
    marker(temp.path(), "start-receipt-durable");
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let identity = record.guardian.unwrap();
    for fd in fs::read_dir(format!("/proc/{}/fd", identity.process.pid)).unwrap() {
        let path = fs::read_link(fd.unwrap().path()).unwrap();
        assert_ne!(path, temp.path().join("owner.lock"));
    }
    let dir = directory(temp.path());
    let secret = fs::read_to_string(dir.join("control.token")).unwrap();
    assert!(
        !fs::read_to_string(temp.path().join("journal.json"))
            .unwrap()
            .contains(&secret)
    );
    assert!(
        !fs::read_to_string(dir.join("guardian-state.json"))
            .unwrap()
            .contains(&secret)
    );
    kill(&mut child);
    assert!(recover(temp.path()).status.success());
}
#[test]
fn unavailable_profile_refuses_before_runtime_state() {
    let temp = TempDir::new().unwrap();
    let result = command(temp.path())
        .env("AQ_WORKER_DISABLE_GUARDIAN", "1")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!temp.path().join("journal.json").exists());
}

#[test]
fn stalled_control_cannot_start_after_expiry_and_stale_identity_is_refused() {
    use std::{
        io::{Read, Write},
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixStream},
        },
    };
    let temp = TempDir::new().unwrap();
    let offset = temp.path().join("offset");
    let read_marker = temp.path().join("control-read");
    fs::write(&offset, "0").unwrap();
    let mut c = command(temp.path());
    c.args([
        "--fault-checkpoint",
        "start-intent-durable",
        "--fault-action",
        "pause",
    ])
    .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset)
    .env("AQ_GUARDIAN_TEST_READ_MS", "500")
    .env("AQ_GUARDIAN_TEST_READ_MARKER", &read_marker);
    let mut child = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    marker(temp.path(), "start-intent-durable");
    let dir = directory(temp.path());
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let identity = record.guardian.unwrap();
    let mut stale = identity.clone();
    stale.attempt.generation += 1;
    assert!(
        aq_worker::guardian::control(
            &dir,
            &stale,
            aq_worker::guardian::Operation::Start {
                grant_sequence: 1,
                deadline_ms: record.grant_deadline_ms.unwrap()
            },
            || Ok(())
        )
        .is_err()
    );
    assert!(!dir.join("started.claim").exists());
    let _ = fs::remove_file(&read_marker);
    let name = format!(
        "aq-worker-guardian-{}-{}",
        unsafe { libc::geteuid() },
        identity.launch_id
    );
    let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let mut stream = UnixStream::connect_addr(&address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let token = fs::read_to_string(dir.join("control.token")).unwrap();
    let body = serde_json::json!({"token":token,"identity":identity,"operation":{"command":"start","grant_sequence":1,"deadline_ms":record.grant_deadline_ms.unwrap()}});
    stream
        .write_all(&serde_json::to_vec(&body).unwrap())
        .unwrap();
    wait(|| read_marker.exists().then_some(()));
    aq_worker::guardian::durable_replace(&offset, b"100000").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut bytes = vec![];
    stream.read_to_end(&mut bytes).unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&bytes)
        .expect("processed request must return a refusal, not pass by timing out");
    assert!(reply["error"].is_string());
    assert!(!dir.join("started.claim").exists());
    kill(&mut child);
    let recovery = recover(temp.path());
    assert!(
        recovery.status.success(),
        "recovery stdout={} stderr={}",
        String::from_utf8_lossy(&recovery.stdout),
        String::from_utf8_lossy(&recovery.stderr)
    );
}

#[test]
fn stalled_renew_cannot_revive_expired_guardian_authority() {
    use std::{
        io::{Read, Write},
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixStream},
        },
    };
    let temp = TempDir::new().unwrap();
    let offset = temp.path().join("offset");
    let read_marker = temp.path().join("control-read");
    fs::write(&offset, "0").unwrap();
    let mut c = command(temp.path());
    c.args([
        "--fault-checkpoint",
        "start-receipt-durable",
        "--fault-action",
        "pause",
    ])
    .env("AQ_FIXTURE_HOLD", "1")
    .env("AQ_FIXTURE_IGNORE_TERM", "1")
    .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset)
    .env("AQ_GUARDIAN_TEST_READ_MS", "500")
    .env("AQ_GUARDIAN_TEST_READ_MARKER", &read_marker);
    let mut child = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    marker(temp.path(), "start-receipt-durable");
    let dir = directory(temp.path());
    let record = state(temp.path()).attempts.into_values().next().unwrap();
    let identity = record.guardian.unwrap();
    let _ = fs::remove_file(&read_marker);
    let name = format!(
        "aq-worker-guardian-{}-{}",
        unsafe { libc::geteuid() },
        identity.launch_id
    );
    let mut stream =
        UnixStream::connect_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
            .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let token = fs::read_to_string(dir.join("control.token")).unwrap();
    let body = serde_json::json!({"token":token,"identity":identity,"operation":{"command":"renew","grant_sequence":2,"deadline_ms":record.grant_deadline_ms.unwrap()+1000}});
    stream
        .write_all(&serde_json::to_vec(&body).unwrap())
        .unwrap();
    wait(|| read_marker.exists().then_some(()));
    aq_worker::guardian::durable_replace(&offset, b"100000").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut bytes = vec![];
    stream.read_to_end(&mut bytes).unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&bytes).expect("renew request must return an actual refusal");
    assert!(reply["error"].is_string());
    let proof: GuardianState = wait(|| {
        serde_json::from_slice::<GuardianState>(&fs::read(dir.join("guardian-state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == GuardianPhase::Stopped)
    });
    assert_eq!(proof.grant_sequence, 1);
    assert_eq!(proof.stop_reason, Some(ProcessOutcome::Expired));
    assert!(proof.kernel_wait_echild);
    kill(&mut child);
    assert!(recover(temp.path()).status.success());
}
#[test]
fn slow_drip_control_has_total_deadline_and_cannot_starve_expiry() {
    use std::{
        io::Write,
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixStream},
        },
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    let temp = TempDir::new().unwrap();
    let offset = temp.path().join("offset");
    let read_marker = temp.path().join("control-read");
    fs::write(&offset, "0").unwrap();
    let mut c = command(temp.path());
    c.args([
        "--fault-checkpoint",
        "start-receipt-durable",
        "--fault-action",
        "pause",
    ])
    .env("AQ_FIXTURE_HOLD", "1")
    .env("AQ_FIXTURE_IGNORE_TERM", "1")
    .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset)
    .env("AQ_GUARDIAN_TEST_READ_MS", "200")
    .env("AQ_GUARDIAN_TEST_READ_MARKER", &read_marker);
    let mut child = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    marker(temp.path(), "start-receipt-durable");
    let dir = directory(temp.path());
    let identity = state(temp.path())
        .attempts
        .into_values()
        .next()
        .unwrap()
        .guardian
        .unwrap();
    let _ = fs::remove_file(&read_marker);
    let name = format!(
        "aq-worker-guardian-{}-{}",
        unsafe { libc::geteuid() },
        identity.launch_id
    );
    let stream =
        UnixStream::connect_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
            .unwrap();
    let mut sender = stream.try_clone().unwrap();
    let dripping = Arc::new(AtomicBool::new(true));
    let running = dripping.clone();
    let thread = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(2);
        while running.load(Ordering::SeqCst) && Instant::now() < end {
            let _ = sender.write(b" ");
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    wait(|| read_marker.exists().then_some(()));
    let start = Instant::now();
    aq_worker::guardian::durable_replace(&offset, b"100000").unwrap();
    let proof: GuardianState = wait(|| {
        serde_json::from_slice::<GuardianState>(&fs::read(dir.join("guardian-state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == GuardianPhase::Stopped)
    });
    assert!(
        start.elapsed() < Duration::from_millis(900),
        "slow drip starved the independent expiry watchdog"
    );
    assert!(dripping.load(Ordering::SeqCst));
    assert!(proof.kernel_wait_echild);
    assert_eq!(proof.stop_reason, Some(ProcessOutcome::Expired));
    dripping.store(false, Ordering::SeqCst);
    thread.join().unwrap();
    drop(stream);
    kill(&mut child);
    assert!(recover(temp.path()).status.success());
}
