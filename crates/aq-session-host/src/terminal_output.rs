//! Bounded tmux control-protocol bytes. A decoded tail is not renderer state.
use super::*;
pub const MAX_CONTROL_LINE: usize = 256 * 1024;
pub const MAX_CONTROL_READ: usize = 64 * 1024;
pub const MAX_CONTROL_EVENTS: usize = 64;
pub enum ControlEvent {
    Bytes(Vec<u8>),
    Paused,
    Continued,
    LayoutChanged,
    Exited,
    CommandError,
    Attached,
}
impl std::fmt::Debug for ControlEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(v) => f.debug_tuple("Bytes").field(&v.len()).finish(),
            Self::Paused => f.write_str("Paused"),
            Self::Continued => f.write_str("Continued"),
            Self::LayoutChanged => f.write_str("LayoutChanged"),
            Self::Exited => f.write_str("Exited"),
            Self::CommandError => f.write_str("CommandError"),
            Self::Attached => f.write_str("Attached"),
        }
    }
}
pub struct ControlDecoder {
    pane: Vec<u8>,
    pending: Vec<u8>,
    block: Option<Vec<u8>>,
    failed: bool,
}
impl ControlDecoder {
    pub fn new(pane: &str) -> Result<Self> {
        ensure!(
            pane.len() <= 32
                && pane
                    .strip_prefix('%')
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
            "control pane identity"
        );
        Ok(Self {
            pane: pane.as_bytes().to_vec(),
            pending: vec![],
            block: None,
            failed: false,
        })
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<ControlEvent>> {
        ensure!(!self.failed, "control decoder is discontinuous");
        let result = self.consume(bytes);
        if result.is_err() {
            self.failed = true;
            self.pending.clear();
        }
        result
    }
    pub fn finish(&mut self) -> Result<()> {
        ensure!(
            !self.failed && self.pending.is_empty() && self.block.is_none(),
            "truncated control stream"
        );
        Ok(())
    }
    fn consume(&mut self, bytes: &[u8]) -> Result<Vec<ControlEvent>> {
        ensure!(bytes.len() <= MAX_CONTROL_READ, "control read byte bound");
        let mut events = Vec::new();
        for piece in bytes.split_inclusive(|b| *b == b'\n') {
            ensure!(
                self.pending.len() + piece.len() <= MAX_CONTROL_LINE,
                "control line byte bound"
            );
            self.pending.extend_from_slice(piece);
            if piece.last() != Some(&b'\n') {
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            line.pop();
            if let Some(event) = self.line(&line)? {
                ensure!(
                    events.len() < MAX_CONTROL_EVENTS,
                    "control event count bound"
                );
                events.push(event);
            }
        }
        Ok(events)
    }
    fn line(&mut self, line: &[u8]) -> Result<Option<ControlEvent>> {
        if let Some(header) = line.strip_prefix(b"%begin ") {
            ensure!(self.block.is_none(), "nested control block");
            validate_header(header)?;
            self.block = Some(header.to_vec());
            return Ok(None);
        }
        for prefix in [b"%end ".as_slice(), b"%error ".as_slice()] {
            if let Some(header) = line.strip_prefix(prefix) {
                validate_header(header)?;
                ensure!(
                    self.block.as_deref() == Some(header),
                    "control block boundary mismatch"
                );
                self.block = None;
                return Ok(Some(if prefix == b"%error " {
                    ControlEvent::CommandError
                } else {
                    ControlEvent::Attached
                }));
            }
        }
        if self.block.is_some() {
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix(b"%output ") {
            let (pane, value) = field(rest)?;
            self.check_pane(pane)?;
            return Ok(Some(ControlEvent::Bytes(unescape(value)?)));
        }
        if let Some(rest) = line.strip_prefix(b"%extended-output ") {
            let (pane, rest) = field(rest)?;
            self.check_pane(pane)?;
            let (age, rest) = field(rest)?;
            number(age)?;
            let value = if let Some(value) = rest.strip_prefix(b": ") {
                value
            } else {
                let at = rest
                    .windows(3)
                    .position(|v| v == b" : ")
                    .context("extended output separator")?;
                ensure!(at <= 128, "extended output header bound");
                &rest[at + 3..]
            };
            return Ok(Some(ControlEvent::Bytes(unescape(value)?)));
        }
        for (prefix, event) in [
            (b"%pause ".as_slice(), ControlEvent::Paused),
            (b"%continue ".as_slice(), ControlEvent::Continued),
        ] {
            if let Some(pane) = line.strip_prefix(prefix) {
                self.check_pane(pane)?;
                return Ok(Some(event));
            }
        }
        if line == b"%exit" || line.starts_with(b"%exit ") {
            return Ok(Some(ControlEvent::Exited));
        }
        if line.starts_with(b"%layout-change ") {
            return Ok(Some(ControlEvent::LayoutChanged));
        }
        ensure!(
            line.is_empty()
                || (line.first() == Some(&b'%') && !line.iter().any(|b| *b < 0x20 || *b == 0x7f)),
            "unexpected control framing"
        );
        Ok(None)
    }
    fn check_pane(&self, pane: &[u8]) -> Result<()> {
        ensure!(pane == self.pane, "foreign control pane");
        Ok(())
    }
}
fn field(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let at = bytes
        .iter()
        .position(|b| *b == b' ')
        .context("control field missing")?;
    ensure!(at > 0, "empty control field");
    Ok((&bytes[..at], &bytes[at + 1..]))
}
fn number(bytes: &[u8]) -> Result<u64> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= 20 && bytes.iter().all(u8::is_ascii_digit),
        "control numeric field"
    );
    std::str::from_utf8(bytes)?
        .parse()
        .context("control number overflow")
}
fn validate_header(bytes: &[u8]) -> Result<()> {
    let values: Vec<_> = bytes.split(|b| *b == b' ').collect();
    ensure!(values.len() == 3, "control block fields");
    for value in values {
        number(value)?;
    }
    Ok(())
}
fn unescape(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\\' {
            let digits = bytes
                .get(at + 1..at + 4)
                .context("truncated control escape")?;
            ensure!(
                digits.iter().all(|b| (b'0'..=b'7').contains(b)),
                "invalid control escape"
            );
            let value = (u16::from(digits[0] - b'0') << 6)
                | (u16::from(digits[1] - b'0') << 3)
                | u16::from(digits[2] - b'0');
            ensure!(value <= 255, "control escape overflow");
            out.push(value as u8);
            at += 4;
        } else {
            ensure!(byte >= 0x20 && byte != 0x7f, "unescaped control byte");
            out.push(byte);
            at += 1;
        }
        ensure!(
            out.len() <= MAX_CONTROL_READ,
            "decoded control output bound"
        );
    }
    Ok(out)
}

/// A read-only client, not a writer guardian or stopped-writer proof. It never
/// sends control commands on stdin; that pipe is held only to avoid client EOF.
pub struct OutputObserver {
    terminal: terminal_io::TerminalHandle,
    child: Option<Child>,
    slot: Option<reaper::Slot>,
    stdout: std::process::ChildStdout,
    _stdin: std::process::ChildStdin,
    decoder: ControlDecoder,
    process: ProcessEvidence,
    failed: bool,
    attached: bool,
}
impl OutputObserver {
    pub fn open(terminal: &terminal_io::TerminalHandle) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let decoder = ControlDecoder::new(&terminal.binding().pane.pane_id)?;
        let slot = reaper::Slot::reserve()?;
        let (directory, tmux, _) = terminal.output_settings()?;
        terminal.claim_output()?;
        terminal.output_settings()?;
        let mut child = Command::new(tmux)
            .args(["-N", "-C", "-S"])
            .arg(directory.join("tmux.sock"))
            .args([
                "attach-session",
                "-f",
                "read-only,ignore-size,pause-after=1",
                "-t",
                "aq",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let setup = (|| -> Result<_> {
            let stdout = child.stdout.take().context("output observer stdout")?;
            let stdin = child.stdin.take().context("output observer stdin")?;
            let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe {
                        libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                    } == 0,
                "output observer nonblocking pipe"
            );
            let process = ownership::process_evidence(child.id())?;
            Ok((stdout, stdin, process))
        })();
        let (stdout, stdin, process) = match setup {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                let _ = slot.track(child);
                return Err(error);
            }
        };
        Ok(Self {
            terminal: terminal.clone(),
            child: Some(child),
            slot: Some(slot),
            stdout,
            _stdin: stdin,
            decoder,
            process,
            failed: false,
            attached: false,
        })
    }
    pub fn process(&self) -> &ProcessEvidence {
        &self.process
    }
    pub fn is_attached(&self) -> bool {
        self.attached && !self.failed
    }
    pub fn terminal(&self) -> &aq_session_protocol::TerminalIdentity {
        &self.terminal.binding().terminal
    }
    pub fn policy(&self) -> Result<aq_session_protocol::TerminalPolicy> {
        Ok(self.terminal.output_settings()?.2)
    }
    /// At most one64KiB nonblocking read. A caller must keep draining while
    /// network I/O is stalled; bounded queues may discard sequenced tail data.
    pub fn poll(&mut self) -> Result<Vec<ControlEvent>> {
        ensure!(!self.failed, "output observer is discontinuous");
        let result = self.read_once();
        if result.is_err() {
            self.failed = true;
            self.close();
        }
        result
    }
    fn read_once(&mut self) -> Result<Vec<ControlEvent>> {
        self.terminal.output_settings()?;
        let mut bytes = [0u8; MAX_CONTROL_READ];
        let events = match self.stdout.read(&mut bytes) {
            Ok(0) => {
                self.decoder.finish()?;
                vec![ControlEvent::Exited]
            }
            Ok(n) => self.decoder.feed(&bytes[..n])?,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                vec![]
            }
            Err(_) => bail!("output observer read failed"),
        };
        if events.iter().any(|e| matches!(e, ControlEvent::Attached)) {
            self.attached = true;
        }
        self.terminal.output_settings()?;
        if events.iter().any(|e| {
            matches!(
                e,
                ControlEvent::Paused | ControlEvent::Exited | ControlEvent::CommandError
            )
        }) {
            self.failed = true;
            self.close();
        }
        Ok(events)
    }
    fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            if !matches!(child.try_wait(), Ok(Some(_))) {
                if let Some(slot) = self.slot.take() {
                    let _ = slot.track(child);
                }
            } else {
                self.slot.take();
            }
        }
    }
}
impl Drop for OutputObserver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode_ansi_octal_and_literal_backslash_without_interpretation() {
        let wire = b"%begin 1 2 0\n%output command-body\n%end 1 2 0\n%output %0 \\033[?1049h\\351\\233\\252\\360\\237\\231\\202\\134\\015\\012\n";
        for size in [1, 2, 7, wire.len()] {
            let mut decoder = ControlDecoder::new("%0").unwrap();
            let mut bytes = vec![];
            for chunk in wire.chunks(size) {
                for event in decoder.feed(chunk).unwrap() {
                    if let ControlEvent::Bytes(v) = event {
                        bytes.extend(v);
                    }
                }
            }
            decoder.finish().unwrap();
            assert_eq!(bytes, "\x1b[?1049h雪🙂\\\r\n".as_bytes());
        }
    }
    #[test]
    fn malformed_foreign_and_oversized_streams_fail_closed() {
        for wire in [
            b"%output %1 private\n".as_slice(),
            b"%output %0 \\999\n",
            b"%output %0 \\400\n",
            b"%end 1 2 0\n",
        ] {
            let mut decoder = ControlDecoder::new("%0").unwrap();
            assert!(decoder.feed(wire).is_err());
            assert!(decoder.feed(b"%output %0 later\n").is_err());
        }
        let mut decoder = ControlDecoder::new("%0").unwrap();
        for _ in 0..4 {
            decoder.feed(&vec![b'x'; MAX_CONTROL_READ]).unwrap();
        }
        assert!(decoder.feed(b"x").is_err());
    }
    #[test]
    fn extended_output_and_pause_are_explicit_not_silent_recovery() {
        let mut decoder = ControlDecoder::new("%0").unwrap();
        let events = decoder
            .feed(b"%extended-output %0 12 : hi\\012\n%pause %0\n%continue %0\n%exit lost\n")
            .unwrap();
        assert!(matches!(&events[0], ControlEvent::Bytes(v) if v == b"hi\n"));
        assert!(matches!(events[1], ControlEvent::Paused));
        assert!(matches!(events[2], ControlEvent::Continued));
        assert!(matches!(events[3], ControlEvent::Exited));
    }
}
