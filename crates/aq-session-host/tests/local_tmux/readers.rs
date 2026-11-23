#[test]
#[ignore = "explicit guardian reader-lease process fixture"]
fn reader_owner_child_fixture() {
    let Some(path) = std::env::var_os("AQ_READER_BINDING") else {
        return;
    };
    let path = PathBuf::from(path);
    let binding: terminal_io::TerminalBinding =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let dir = path.parent().unwrap();
    let host = SessionHost::observe_existing(dir.parent().unwrap(), &binding.guardian).unwrap();
    let io = terminal_io::TerminalHandle::recover(&host, &binding.terminal).unwrap();
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    wait_named("child observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    std::fs::write(
        dir.join("reader-owner-ready"),
        serde_json::to_vec(observer.process()).unwrap(),
    )
    .unwrap();
    loop {
        if observer.poll().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
#[ignore = "explicit local tmux guardian read-lease qualification"]
fn quiet_drained_reader_renews_but_undrained_reader_expires_without_stopping_harness() {
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    let end = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < end {
        observer.poll().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ownership::pidfd_is_live(&client).unwrap());
    let input = io_input(&lease, b"flood\n");
    let id = input.operation_id;
    io.submit(
        &aq_terminal_relay_protocol::Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    // Deliberately do not read the pipe or issue drain receipts.
    wait_named("undrained reader independently severed", || {
        (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
    });
    wait_named("producer progresses after undrained cutoff", || {
        std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v >= 4 * 1024 * 1024)
    });
    assert!(matches!(
        io.receipt(id).unwrap(),
        Some(terminal_io::Receipt::Input(
            aq_session_protocol::InputReceipt {
                disposition: aq_session_protocol::InputDisposition::Applied,
                ..
            }
        ))
    ));
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    assert!(observer.poll().is_err());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}
/// On a busy machine the guardian can be held between reading its clock and
/// receiving a drain receipt. A receipt stamped in that gap is fresh, not from
/// the future: refusing it closed the one-shot reader, and the attempt's
/// terminal output stopped for good while the agent kept working.
#[test]
#[ignore = "explicit local tmux guardian read-lease qualification"]
fn drain_receipt_stamped_during_a_slow_guardian_tick_keeps_the_reader() {
    let (_case, mut host, io, _lease) = io_setup_mode("output-flood");
    std::fs::write(host.directory().join("test-reader-receive-delay"), b"").unwrap();
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    let end = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < end {
        observer.poll().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ownership::pidfd_is_live(&client).unwrap());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}
#[test]
#[ignore = "explicit local tmux independent owner failure qualification"]
fn dead_or_stopped_owner_and_lost_ready_or_ack_cannot_leave_a_blocking_reader() {
    for mode in ["dead", "stopped", "before-ready", "before-ack"] {
        let (_case, mut host, io, lease) = io_setup_mode("output-flood");
        let mut consumer = spawn_reader_owner(&host, mode);
        let process: ProcessEvidence = wait_named("owned read client created", || {
            std::fs::read(host.directory().join("reader.process.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
        });
        let client = ownership::verified_pidfd(&process).unwrap();
        lose_reader_owner(&host, mode, &mut consumer);
        let input = io_input(&lease, b"flood\n");
        let id = input.operation_id;
        io.submit(
            &aq_terminal_relay_protocol::Effect::Input(input),
            terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
        )
        .unwrap();
        wait_named("reader severed after owner/handoff failure", || {
            (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
        });
        wait_named("harness progresses independently of owner", || {
            std::fs::read_to_string(host.directory().join("fixture-output-progress"))
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v >= 4 * 1024 * 1024)
        });
        assert!(matches!(
            io.receipt(id).unwrap(),
            Some(terminal_io::Receipt::Input(
                aq_session_protocol::InputReceipt {
                    disposition: aq_session_protocol::InputDisposition::Applied,
                    ..
                }
            ))
        ));
        assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
        assert!(terminal_output::OutputObserver::open(&io).is_err());
        if mode != "dead" {
            consumer.kill().unwrap();
        }
        consumer.wait().unwrap();
        host.stop().unwrap();
        let proof = host.wait_stopped(Duration::from_secs(3)).unwrap();
        assert!(proof.kernel_wait_echild);
    }
}

#[test]
#[ignore = "explicit actual slow-consumer read-lease qualification"]
fn slow_data_only_polls_cannot_renew_an_undrained_reader_forever() {
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    wait_named("slow observer attached", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    io.submit(
        &aq_terminal_relay_protocol::Effect::Input(io_input(&lease, b"flood\n")),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    wait_named("synthetic flood consumed", || {
        host.directory()
            .join("fixture-output-start")
            .exists()
            .then_some(())
    });
    let started = Instant::now();
    let mut data_polls = 0;
    let mut continued_packets = 0;
    while ownership::pidfd_is_live(&client).unwrap() {
        std::thread::sleep(Duration::from_millis(200));
        match observer.poll() {
            Ok(events) => {
                assert!(events.len() <= terminal_output::MAX_CONTROL_EVENTS);
                assert!(observer.buffered_wire_bytes() <= terminal_output::MAX_CONTROL_READ);
                data_polls += usize::from(
                    events
                        .iter()
                        .any(|e| matches!(e, terminal_output::ControlEvent::Bytes(_))),
                );
                continued_packets += usize::from(observer.buffered_wire_bytes() > 0);
            }
            Err(_) => break,
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "slow data-only reader kept renewing"
        );
    }
    assert!(data_polls > 0);
    wait_named("slow reader cutoff", || {
        (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
    });
    wait_named("harness progresses after slow reader cutoff", || {
        std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v >= 4 * 1024 * 1024)
    });
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    eprintln!(
        "slow data-only cutoff={:?}, data_polls={data_polls}, retained_packet_polls={continued_packets}",
        started.elapsed()
    );
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}

fn spawn_reader_owner(host: &SessionHandle, mode: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "reader_owner_child_fixture",
            "--test-threads=1",
        ])
        .env(
            "AQ_READER_BINDING",
            host.directory().join("terminal-binding.json"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if mode.starts_with("before-") {
        command.env("AQ_HOST_READER_PAUSE", mode);
    }
    command.spawn().unwrap()
}
fn lose_reader_owner(host: &SessionHandle, mode: &str, consumer: &mut Child) {
    if mode.starts_with("before-") {
        wait_named("private handoff pause reached", || {
            host.directory()
                .join("reader-owner-paused")
                .exists()
                .then_some(())
        });
    } else {
        wait_named("reader consumer ready", || {
            host.directory()
                .join("reader-owner-ready")
                .exists()
                .then_some(())
        });
        if mode == "dead" {
            consumer.kill().unwrap();
        } else {
            unsafe {
                libc::kill(consumer.id() as i32, libc::SIGSTOP);
            }
        }
    }
}
