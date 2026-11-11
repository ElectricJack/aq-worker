use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    process::Stdio,
};
use tokio::{
    io::unix::AsyncFd,
    process::{Child, Command},
};
/// Only this Podman/tmux attach is disposable. Its detached agent is untouched.
pub(crate) struct Pty {
    master: AsyncFd<File>,
    _child: Child,
}
impl Pty {
    pub fn attach(
        exe: &Path,
        socket: &Path,
        container: &str,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        let mut cmd = Command::new(exe);
        cmd.args(["--remote", "--url"])
            .arg(format!("unix://{}", socket.display()))
            .args([
                "exec",
                "--env",
                "TERM=xterm-256color",
                "--env",
                "COLORTERM=truecolor",
                "-it",
                container,
                "tmux",
                "-u",
                "-S",
                "/aq/out/manual.sock",
                "set-window-option",
                "-t",
                "aq",
                "window-size",
                "latest",
                ";",
                "attach-session",
                "-E",
                "-t",
                "aq",
            ]);
        Self::command(cmd, columns, rows)
    }
    pub fn command(mut cmd: Command, columns: u16, rows: u16) -> Result<Self> {
        let (mut master, mut slave) = (-1, -1);
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
            "terminal PTY allocation failed"
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            ensure!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "PTY descriptor protection failed"
            );
        }
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                } == 0,
            "PTY async configuration failed"
        );
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        ensure!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), termios.as_mut_ptr()) } == 0,
            "PTY settings unavailable"
        );
        let mut termios = unsafe { termios.assume_init() };
        unsafe { libc::cfmakeraw(&mut termios) };
        ensure!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) } == 0,
            "PTY raw mode failed"
        );
        set_size(&slave, columns, rows)?;
        cmd.env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .kill_on_drop(true);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("start disposable tmux attach")?;
        Ok(Self {
            // SAFETY: AsyncFd owns this File until drop; its descriptor is never replaced.
            master: unsafe { AsyncFd::register(master) }
                .map_err(|_| anyhow::anyhow!("PTY registration failed"))?,
            _child: child,
        })
    }
    pub fn resize(&self, c: u16, r: u16) -> Result<()> {
        set_size(self.master.get_ref(), c, r)
    }
    pub async fn read(&self) -> Result<Vec<u8>> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| {
                let mut bytes = vec![0; 16384];
                let mut file = fd.get_ref();
                let n = file.read(&mut bytes)?;
                bytes.truncate(n);
                Ok(bytes)
            }) {
                Ok(result) => {
                    let bytes = result?;
                    ensure!(!bytes.is_empty(), "tmux attach ended");
                    return Ok(bytes);
                }
                Err(_) => continue,
            }
        }
    }
    pub async fn write(&self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| {
                let mut file = fd.get_ref();
                file.write(bytes)
            }) {
                Ok(result) => {
                    let n = result?;
                    ensure!(n > 0, "tmux input closed");
                    bytes = &bytes[n..]
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }
}
fn set_size(file: &File, c: u16, r: u16) -> Result<()> {
    ensure!(
        (2..=500).contains(&c) && (1..=300).contains(&r),
        "terminal dimensions invalid"
    );
    let size = libc::winsize {
        ws_col: c,
        ws_row: r,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    ensure!(
        unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0,
        "PTY resize failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn persistent_pty_preserves_cursor_bytes_and_sends_input_without_spawning_per_key() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fixed-attach");
        std::fs::write(
            &executable,
            b"#!/bin/sh\nstty size\nprintf '\\033[3;6Hready'\nexec cat\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = Pty::attach(&executable, Path::new("/unused"), "fixture", 90, 31).unwrap();
        let mut output = Vec::new();
        while !output.ends_with(b"ready") {
            output.extend(
                tokio::time::timeout(std::time::Duration::from_secs(2), p.read())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(output.windows(5).any(|w| w == b"31 90"));
        assert!(output.ends_with(b"\x1b[3;6Hready"));
        p.write(b"abc\x1b[D!").await.unwrap();
        let echo = tokio::time::timeout(std::time::Duration::from_secs(2), p.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echo, b"abc\x1b[D!");
        p.resize(101, 37).unwrap();
        let mut size = libc::winsize {
            ws_col: 0,
            ws_row: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe { libc::ioctl(p.master.get_ref().as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
            0
        );
        assert_eq!((size.ws_col, size.ws_row), (101, 37));
    }
}
