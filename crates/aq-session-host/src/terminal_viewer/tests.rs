use super::*;

const TMUX: &str = "/usr/bin/tmux";
const MARKER: &str = "actual-pty-first-draw";
struct TmuxServer(PathBuf);
impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new(TMUX)
            .args(["-N", "-S"])
            .arg(&self.0)
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl TmuxServer {
    fn start(socket: PathBuf) -> Self {
        let cleanup = Self(socket);
        let start = Command::new(TMUX)
            .args(["-S"])
            .arg(&cleanup.0)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                "viewer-proof",
                r"printf '\033[2J\033[Hactual-pty-first-draw'; exec /bin/sleep 30",
            ])
            .output()
            .unwrap();
        assert!(start.status.success());
        cleanup
    }
    fn query(&self, args: &[&str]) -> String {
        let value = Command::new(TMUX)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-N", "-S"])
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(value.status.success());
        String::from_utf8(value.stdout).unwrap()
    }
    fn verify_flags(&self, pid: u32, session: &str) {
        assert!(
            self.query(&["list-clients", "-F", CLIENT_METADATA])
                .lines()
                .any(|line| line == format!("{}|{}", pid, session))
        );
        assert!(
            self.query(&[
                "list-clients",
                "-F",
                "#{client_pid}|#{session_id}|#{client_flags}"
            ])
            .lines()
            .any(|line| line.starts_with(&format!("{}|{}|", pid, session))
                && line.contains("read-only")
                && line.contains("ignore-size"))
        );
    }
    fn pane(&self) -> (String, ProcessEvidence) {
        let session = self
            .query(&[
                "display-message",
                "-p",
                "-t",
                "viewer-proof:",
                "#{session_id}",
            ])
            .trim()
            .to_owned();
        let pid: u32 = self
            .query(&["display-message", "-p", "-t", &session, "#{pane_pid}"])
            .trim()
            .parse()
            .unwrap();
        let pane = ownership::process_evidence(pid).unwrap();
        let metadata = self.query(&["display-message", "-p", "-t", &session, PANE_METADATA]);
        let fields: Vec<_> = metadata.trim_end_matches('\n').split('|').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], session);
        assert_eq!(fields[1], "viewer-proof");
        assert_eq!(fields[3].parse::<u32>().unwrap(), pid);
        assert_eq!(&fields[4..], ["80", "24"]);
        (session, pane)
    }
    fn wait_idle(&self, session: &str) {
        let ready = Instant::now() + Duration::from_secs(2);
        while !self
            .query(&["capture-pane", "-p", "-t", session])
            .contains(MARKER)
        {
            assert!(Instant::now() < ready);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn verify_redraw(output: &mut File) {
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(
        unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let end = Instant::now() + Duration::from_secs(2);
    let mut received = Vec::new();
    loop {
        let mut bytes = [0u8; MAX_READ];
        match output.read(&mut bytes) {
            Ok(n) if n > 0 => received.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            _ => panic!("PTY viewer ended before redraw"),
        }
        if received
            .windows(MARKER.len())
            .any(|v| v == MARKER.as_bytes())
        {
            break;
        }
        assert!(received.len() <= 65_536 && Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_detach(mut child: Child) {
    let end = Instant::now() + Duration::from_secs(1);
    while child.try_wait().unwrap().is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

// Real tmux/PTY regression for the V1 port. It exercises the read-only
// attach primitive, not a grant, baseline, provider or full-history claim.
#[test]
fn actual_readonly_pty_attach_redraws_idle_pane_and_detach_preserves_process() {
    let temp = tempfile::tempdir().unwrap();
    let tmux = TmuxServer::start(temp.path().join("tmux.sock"));
    let (session, pane) = tmux.pane();
    tmux.wait_idle(&session);
    // Each attachment must paint existing idle text without new control events.
    for _ in 0..2 {
        let (child, viewer, mut output) =
            spawn_attach(Path::new(TMUX), &tmux.0, &session, (80, 24)).unwrap();
        verify_redraw(&mut output);
        tmux.verify_flags(child.id(), &session);
        drop(output);
        drop(viewer);
        wait_detach(child);
        assert_eq!(ownership::process_evidence(pane.pid).unwrap(), pane);
        assert_eq!(
            tmux.query(&["display-message", "-p", "-t", &session, "#{session_id}"])
                .trim(),
            session
        );
    }
}
