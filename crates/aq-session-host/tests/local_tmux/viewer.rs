#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn read_only_output_preserves_unicode_alternate_bytes_and_does_not_resize_or_restart() {
    use aq_session_host::terminal_output::OutputObserver;
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup_mode("output-pattern");
    let before = pane_geometry(&host);
    let mut observer = OutputObserver::open(&io).unwrap();
    wait(|| {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    assert_eq!(before, pane_geometry(&host));
    let input = io_input(&lease, b"emit\n");
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    wait_for_pattern(&mut observer);
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
    record_compatibility_evidence(&case, &host);
}

/// The live terminal relay's viewer for a queued attempt: an ordinary tmux client
/// on the attempt's session, on its own PTY, not spawned by the guardian. Attaching
/// paints the screen as it is and its keys reach the pane program. Its size
/// cannot change the pane, and detaching keeps the host running.
#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn an_ordinary_viewer_paints_and_types_without_resizing_the_host() {
    use aq_terminal_relay_protocol::Effect;
    use std::io::Write;
    let (_case, mut host, io, lease) = io_setup();
    io.submit(
        &Effect::Input(io_input(&lease, b"already-on-screen\n")),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    wait_named("first input", || {
        io_lines(&host)
            .contains(&"already-on-screen".to_string())
            .then_some(())
    });

    // As in production, the output observer's control client is attached as well.
    let open = || aq_session_host::terminal_output::OutputObserver::open(&io).ok();
    let mut observer = wait_named("output observer admission", open);
    wait_named("output observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    assert_eq!(
        tmux(&host, &["show-options", "-gv", "prefix"]).stdout,
        b"None\n"
    );
    assert_eq!(pane_width(&host), b"80\n");
    let (mut master, slave) = viewer_pty();
    let mut viewer = spawn_viewer(&host, slave);
    wait_for_viewer_paint(&mut master);
    master.write_all(b"typed-by-viewer\n").unwrap();
    wait_named("viewer typing", || {
        io_lines(&host)
            .contains(&"typed-by-viewer".to_string())
            .then_some(())
    });
    assert_eq!(pane_width(&host), b"80\n");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    viewer.kill().unwrap();
    viewer.wait().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    io.submit(
        &Effect::Input(io_input(&lease, b"after-viewer\n")),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    wait_named("input after viewer", || {
        io_lines(&host)
            .contains(&"after-viewer".to_string())
            .then_some(())
    });
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

fn pane_geometry(host: &SessionHandle) -> Vec<u8> {
    tmux(
        host,
        &[
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{window_width}x#{window_height}",
        ],
    )
    .stdout
}
fn pane_width(host: &SessionHandle) -> Vec<u8> {
    tmux(
        host,
        &["display-message", "-p", "-t", "aq:0.0", "#{window_width}"],
    )
    .stdout
}

fn record_compatibility_evidence(case: &Case, host: &SessionHandle) {
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
fn viewer_pty() -> (std::fs::File, std::fs::File) {
    use std::{
        fs::File,
        os::fd::{AsRawFd, FromRawFd},
    };
    let (mut master, mut slave) = (-1, -1);
    let (name, termios, winsize) = (std::ptr::null_mut(), std::ptr::null(), std::ptr::null());
    assert_eq!(
        unsafe { libc::openpty(&mut master, &mut slave, name, termios, winsize) },
        0
    );
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    (master, slave)
}
fn spawn_viewer(host: &SessionHandle, slave: std::fs::File) -> Child {
    use std::os::unix::process::CommandExt;
    let mut viewer = Command::new("/usr/bin/tmux");
    viewer
        .args(["-N", "-u", "-S"])
        .arg(host.directory().join("tmux.sock"))
        .args(["attach-session", "-t", "aq"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        viewer.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    viewer.spawn().unwrap()
}
fn wait_for_viewer_paint(master: &mut std::fs::File) {
    use std::io::Read;
    let mut painted = Vec::new();
    wait_named("viewer paint", || {
        let mut bytes = [0u8; 16384];
        if let Ok(n) = master.read(&mut bytes) {
            painted.extend_from_slice(&bytes[..n]);
        }
        String::from_utf8_lossy(&painted)
            .contains("already-on-screen")
            .then_some(())
    });
}
fn wait_for_pattern(observer: &mut terminal_output::OutputObserver) {
    use terminal_output::ControlEvent;
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
}
