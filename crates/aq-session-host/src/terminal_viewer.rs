//! Disposable, read-only tmux PTY attachment. Bytes are tmux's real redraw,
//! never a capture-text reconstruction, Snapshot or complete-history claim.
use super::*;
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::process::CommandExt,
};
use terminal_io::TerminalHost;
use terminal_output::ControlEvent;

const MAX_READ: usize = 16 * 1024;
// tmux sanitizes control characters in metadata formats without a terminal
// environment. These validated IDs and the fixed session name use a printable
// delimiter so the guardian's cleared environment preserves exact boundaries.
const PANE_METADATA: &str =
    "#{session_id}|#{session_name}|#{pane_id}|#{pane_pid}|#{pane_width}|#{pane_height}";
const CLIENT_METADATA: &str = "#{client_pid}|#{session_id}";

/// Actual running terminal owner is required; no serialized constructor or
/// input writer is exposed. Dropping the handle detaches only this viewer.
pub struct TerminalViewer {
    terminal: terminal_io::TerminalHandle,
    lease: reader_lease::Client,
    output: File,
    failed: bool,
}
impl TerminalViewer {
    pub fn open(terminal: &terminal_io::TerminalHandle) -> Result<Self> {
        let claim = terminal.viewer_claim()?;
        let (lease, output, _) = reader_lease::Client::open_viewer(terminal, claim)?;
        Ok(Self {
            terminal: terminal.clone(),
            lease,
            output,
            failed: false,
        })
    }
    pub fn poll(&mut self) -> Result<Vec<ControlEvent>> {
        ensure!(!self.failed, "PTY viewer disconnected");
        let result = self.read_once();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn read_once(&mut self) -> Result<Vec<ControlEvent>> {
        self.terminal.viewer_claim()?;
        self.lease.check()?;
        let mut bytes = [0u8; MAX_READ];
        match self.output.read(&mut bytes) {
            Ok(0) => bail!("PTY viewer detached"),
            Ok(n) => Ok(vec![ControlEvent::Bytes(bytes[..n].to_vec())]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.lease.drained()?;
                Ok(vec![])
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(vec![]),
            Err(_) => bail!("PTY viewer read unavailable"),
        }
    }
}

pub(crate) struct ViewerProcess {
    master: File,
    session: String,
    size: (u16, u16),
    checked: Instant,
}
impl ViewerProcess {
    pub(crate) fn tick(&mut self, target: &TerminalHost<'_>, pid: u32) -> Result<()> {
        if self.checked.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        let (session, columns, rows) = target.pane()?;
        ensure!(session == self.session, "PTY viewer session changed");
        target.verify_client(pid, &session)?;
        // Follow an already accepted pane Resize; this client has ignore-size
        // and cannot independently resize the harness or acquire control.
        if self.size != (columns, rows) {
            set_size(&self.master, columns, rows)?;
            let evidence = ownership::process_evidence(pid)?;
            let fd = ownership::verified_pidfd(&evidence)?;
            ownership::signal(&fd, libc::SIGWINCH)?;
            self.size = (columns, rows);
        }
        self.checked = Instant::now();
        Ok(())
    }
}
impl TerminalHost<'_> {
    fn query(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.guarded_tmux(self.host.permit_deadline_boot_ms, args)
    }

    fn pane(&self) -> Result<(String, u16, u16)> {
        let p = self.host.pane.as_ref().context("PTY viewer pane absent")?;
        let value = self.query(&["display-message", "-p", "-t", &p.pane_id, PANE_METADATA])?;
        let text = std::str::from_utf8(&value).context("PTY viewer identity encoding")?;
        let fields: Vec<_> = text.trim_end_matches('\n').split('|').collect();
        ensure!(
            fields.len() == 6
                && numeric_session(fields[0])
                && fields[1] == p.session_name
                && fields[2] == p.pane_id
                && fields[3].parse::<u32>().ok() == Some(p.pane.pid),
            "PTY viewer exact pane/session mismatch"
        );
        let columns = fields[4].parse::<u16>()?;
        let rows = fields[5].parse::<u16>()?;
        ensure!(
            // Match ResizeCommand's accepted geometry (session protocol v1).
            // The smaller observed-startup bounds constrain initial layout only;
            // TerminalPolicy has no separate post-start dimension ceiling.
            columns > 0 && rows > 0 && columns <= 1000 && rows <= 1000,
            "PTY viewer dimensions unavailable"
        );
        Ok((fields[0].to_owned(), columns, rows))
    }
    fn verify_client(&self, pid: u32, session: &str) -> Result<()> {
        let policy = self.query(&["show-options", "-Av", "-t", session, "detach-on-destroy"])?;
        ensure!(
            policy == b"on\n",
            "PTY viewer requires detach-on-destroy on"
        );
        let clients = self.query(&["list-clients", "-F", CLIENT_METADATA])?;
        let expected = format!("{pid}|{session}");
        ensure!(
            std::str::from_utf8(&clients)?
                .lines()
                .any(|v| v == expected),
            "PTY viewer client target changed"
        );
        Ok(())
    }
}
fn numeric_session(s: &str) -> bool {
    s.strip_prefix('$')
        .is_some_and(|v| !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit()))
}
pub(crate) fn spawn(target: &TerminalHost<'_>) -> Result<(Child, ViewerProcess, File)> {
    let (session, columns, rows) = target.pane()?;
    let (mut child, viewer, output) = spawn_attach(
        &target.config.tmux,
        &target.dir.join("tmux.sock"),
        &session,
        (columns, rows),
    )?;
    let ready: Result<()> = (|| {
        let end = Instant::now() + Duration::from_millis(250);
        loop {
            if target.verify_client(child.id(), &session).is_ok() {
                // Revalidate the actual pane after the attach became visible.
                ensure!(
                    target.pane()? == (session.clone(), columns, rows),
                    "PTY viewer pane changed during attach"
                );
                return Ok(());
            }
            ensure!(
                child.try_wait()?.is_none() && Instant::now() < end,
                "PTY viewer attachment unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if let Err(error) = ready {
        let _ = child.kill();
        return Err(error);
    }
    Ok((child, viewer, output))
}
fn set_size(file: &File, columns: u16, rows: u16) -> Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    ensure!(
        unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0,
        "PTY viewer sizing unavailable"
    );
    Ok(())
}
fn viewer_pty(size: (u16, u16)) -> Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    ensure!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0,
        "PTY viewer allocation unavailable"
    );
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    // openpty does not set CLOEXEC. Neither end may leak into later launches.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "PTY viewer descriptor protection unavailable"
        );
    }
    set_size(&slave, size.0, size.1)?;
    Ok((master, slave))
}

fn attach_command(tmux: &Path, socket: &Path, session: &str, slave: File) -> Result<Command> {
    let mut command = Command::new(tmux);
    command
        .args(["-N", "-u", "-S"])
        .arg(socket)
        .args([
            "-T",
            "RGB",
            "attach-session",
            "-E",
            "-f",
            "read-only,ignore-size",
            "-t",
            session,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

fn spawn_attach(
    tmux: &Path,
    socket: &Path,
    session: &str,
    size: (u16, u16),
) -> Result<(Child, ViewerProcess, File)> {
    ensure!(
        numeric_session(session),
        "PTY viewer numeric session required"
    );
    let (master, slave) = viewer_pty(size)?;
    let mut command = attach_command(tmux, socket, session, slave)?;
    let output = master.try_clone()?;
    let child = command.spawn()?;
    Ok((
        child,
        ViewerProcess {
            master,
            session: session.to_owned(),
            size,
            checked: Instant::now(),
        },
        output,
    ))
}

#[cfg(test)]
mod tests;
