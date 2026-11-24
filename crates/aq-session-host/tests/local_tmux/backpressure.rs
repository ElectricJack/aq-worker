#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn sole_sigstopped_observer_does_not_block_pty_progress_or_guardian_control() {
    use aq_session_host::terminal_output::OutputObserver;
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = OutputObserver::open(&io).unwrap();
    wait_named("output observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = stop_observed_reader(observer.process());
    let input = io_input(&lease, b"flood\n");
    let operation_id = input.operation_id;
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let progress = match wait_for_output_progress(&host) {
        Ok(progress) => progress,
        Err((progress, elapsed)) => {
            log_stalled_output(&io, operation_id, &host);
            std::mem::forget(_case);
            panic!("stopped observer PTY progress timeout: {progress} bytes after {elapsed:?}");
        }
    };
    assert!(
        !ownership::pidfd_is_live(&client).unwrap(),
        "guardian must sever the stopped read-only client"
    );
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
    let lost = observer.poll();
    assert!(
        lost.is_err()
            || lost
                .unwrap()
                .iter()
                .any(|e| matches!(e, aq_session_host::terminal_output::ControlEvent::Exited)),
        "reader cutoff must invalidate continuity"
    );
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
    let client = stop_observed_reader(observer.process());
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
        !ownership::pidfd_is_live(&client).unwrap(),
        "guardian-owned stopped reader must be reaped too"
    );
    let process = observer.process().clone();
    drop(observer);
    wait(|| {
        (!ownership::verified_pidfd(&process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
        .then_some(())
    });
}

fn wait_for_output_progress(host: &SessionHandle) -> std::result::Result<u64, (u64, Duration)> {
    let stopped = Instant::now();
    let mut progress: u64;
    let progress_deadline = stopped + Duration::from_secs(5);
    loop {
        progress = std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if progress >= 4 * 1024 * 1024 && stopped.elapsed() >= Duration::from_millis(1300) {
            return Ok(progress);
        }
        if Instant::now() >= progress_deadline {
            return Err((progress, stopped.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn log_stalled_output(io: &terminal_io::TerminalHandle, operation_id: Uuid, host: &SessionHandle) {
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
}
