//! Nonblocking command I/O shares one deadline across reads, writes and process exit.
use super::{reaper, supervisor};
use anyhow::{Context, Result, ensure};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn bounded_command(command: &mut Command) -> Result<Vec<u8>> {
    bounded_command_input(command, None, Duration::from_millis(250))
}
pub(super) fn bounded_command_input(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    ensure!(
        timeout <= Duration::from_millis(250) && !timeout.is_zero(),
        "local command deadline bounds"
    );
    ensure!(
        input.is_none_or(|v| v.len() <= 65536),
        "local command input bounds"
    );
    // A guardian remains single-threaded and reaps all its command children in
    // its ownership loop. Other callers reserve parent registry capacity first.
    let slot = if supervisor::in_guardian() {
        None
    } else {
        Some(reaper::Slot::reserve()?)
    };
    let mut child = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = exchange(&mut child, input, timeout);
    if result.is_err() {
        let _ = child.kill();
        if !matches!(child.try_wait(), Ok(Some(_)))
            && let Some(slot) = slot
        {
            let _ = slot.track(child);
        }
    }
    result
}

fn nonblocking(pipe: &impl AsRawFd, stream: &str) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "nonblocking command {stream} unavailable"
    );
    Ok(())
}

fn write_available(
    stdin: &mut Option<ChildStdin>,
    input: Option<&[u8]>,
    written: &mut usize,
) -> Result<()> {
    let (Some(bytes), Some(pipe)) = (input, stdin.as_mut()) else {
        return Ok(());
    };
    match pipe.write(&bytes[*written..]) {
        Ok(n) => *written += n,
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
        Err(e) => return Err(e.into()),
    }
    if *written == bytes.len() {
        stdin.take();
    }
    Ok(())
}

/// Return true once the pipe is closed, false when it would block.
fn read_available(stdout: &mut ChildStdout, output: &mut Vec<u8>) -> Result<bool> {
    let mut buffer = [0u8; 4096];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                ensure!(output.len() + n <= 65536, "local tmux output bounds");
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

fn exchange(child: &mut Child, input: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>> {
    let mut stdin = child.stdin.take();
    if let Some(pipe) = &stdin {
        nonblocking(pipe, "stdin")?;
    }
    let mut stdout = child.stdout.take().context("command stdout")?;
    nonblocking(&stdout, "stdout")?;
    let end = Instant::now() + timeout;
    let mut written = 0;
    let mut output = Vec::new();
    let mut status = None;
    loop {
        ensure!(Instant::now() < end, "local tmux command total deadline");
        write_available(&mut stdin, input, &mut written)?;
        let eof = read_available(&mut stdout, &mut output)?;
        if status.is_none() {
            status = child.try_wait()?;
        }
        if eof && let Some(status) = status {
            ensure!(status.success(), "local tmux command failed");
            return Ok(output);
        }
        ensure!(Instant::now() < end, "local tmux command total deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
