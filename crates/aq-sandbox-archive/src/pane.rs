//! The node driver's side of an interactive sandbox terminal: the hijacked
//! container attach, and the byte relay between it and the session host's pane.
//! Output is redacted before it reaches the pane, so no later relay, tail or
//! history ever holds a credential the node knows.
use crate::redact::Redactor;
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// Masks exact occurrences of known secrets in a byte stream, including one
/// split across reads: a tail that could still become a secret is held back
/// until the next bytes decide it. Bytes that cannot start a secret pass at once.
pub struct ExactRedactor {
    secrets: Vec<Zeroizing<Vec<u8>>>,
    held: Vec<u8>,
    /// A secret was masked.
    pub leaked: bool,
}
impl ExactRedactor {
    pub fn new(secrets: Vec<Zeroizing<Vec<u8>>>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
            held: Vec::new(),
            leaked: false,
        }
    }
    pub fn frame(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(bytes);
        for secret in &self.secrets {
            let mut at = 0;
            while at + secret.len() <= self.held.len() {
                if self.held[at..at + secret.len()] == secret[..] {
                    self.held[at..at + secret.len()].fill(b'*');
                    self.leaked = true;
                    at += secret.len();
                } else {
                    at += 1;
                }
            }
        }
        let held = &self.held;
        let keep = (1..=held.len())
            .rev()
            .find(|n| {
                let tail = &held[held.len() - n..];
                (self.secrets.iter()).any(|s| s.len() > *n && s.starts_with(tail))
            })
            .unwrap_or(0);
        let cut = self.held.len() - keep;
        self.held.drain(..cut).collect()
    }
}

/// The pane the driver runs in. `tty`: its input is a terminal, put in raw
/// mode for the time of the relay.
pub struct Pane {
    pub input: Box<dyn Read + Send>,
    pub output: Box<dyn Write + Send>,
    pub tty: bool,
}
impl Pane {
    /// This process's own terminal (the session host's tmux pane).
    pub fn stdio() -> Self {
        Self {
            input: Box::new(std::io::stdin()),
            output: Box::new(std::io::stdout()),
            // The session host may give the pane program its startup input on stdin; the pane
            // itself is then stdout, and it still has to leave cooked mode (below).
            tty: unsafe { libc::isatty(0) == 1 || libc::isatty(1) == 1 },
        }
    }
}
/// Columns and rows of this process's terminal.
pub fn window_size() -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0)
        .then_some((size.ws_col, size.ws_row))
}
/// The pane's terminal in raw mode. A cooked pane echoes what tmux writes to its input
/// (its replies to the program's DA1/XTVERSION queries) onto the screen as `^[P>|tmux 3.4…`.
struct Raw(libc::termios, i32);
impl Raw {
    fn enter() -> Option<Self> {
        let fd = [0, 1]
            .into_iter()
            .find(|fd| unsafe { libc::isatty(*fd) } == 1)?;
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
            return None;
        }
        let mut raw = old;
        unsafe { libc::cfmakeraw(&mut raw) };
        (unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == 0).then_some(Self(old, fd))
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.1, libc::TCSANOW, &self.0) };
    }
}

/// A running relay. Pane input goes to the sandbox terminal as typed; sandbox
/// output goes to the pane with the known secrets and credential shapes masked.
pub struct Relay {
    leaked: Arc<AtomicBool>,
    output: tokio::task::JoinHandle<()>,
    input: tokio::task::JoinHandle<()>,
    _raw: Option<Raw>,
}
fn pane_input(mut input: Box<dyn Read + Send>) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (keys, typed) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    // A plain thread: a blocked terminal read must never hold the process open.
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        let mut replies = crate::query_replies::ReplyFilter::default();
        while let Ok(n @ 1..) = input.read(&mut buffer) {
            let typed = replies.feed(&buffer[..n]);
            if !typed.is_empty() && keys.blocking_send(typed).is_err() {
                break;
            }
        }
    });
    typed
}

async fn pane_output<R: AsyncRead + Unpin>(
    mut from: R,
    mut out: Box<dyn Write + Send>,
    secrets: Vec<Zeroizing<Vec<u8>>>,
    leaked: Arc<AtomicBool>,
) {
    let (mut exact, mut shapes) = (ExactRedactor::new(secrets), Redactor::default());
    let mut buffer = vec![0u8; 65536];
    while let Ok(n @ 1..) = from.read(&mut buffer).await {
        let frame = shapes.frame(&exact.frame(&buffer[..n]));
        leaked.fetch_or(exact.leaked, Ordering::SeqCst);
        if out.write_all(&frame).and_then(|()| out.flush()).is_err() {
            break;
        }
    }
}

impl Relay {
    pub fn start<S>(stream: S, pane: Pane, secrets: Vec<Zeroizing<Vec<u8>>>) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let raw = pane.tty.then(Raw::enter).flatten();
        let (from, mut to) = tokio::io::split(stream);
        let mut typed = pane_input(pane.input);
        let input = tokio::spawn(async move {
            while let Some(bytes) = typed.recv().await {
                if to.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let leaked = Arc::new(AtomicBool::new(false));
        let output = tokio::spawn(pane_output(from, pane.output, secrets, leaked.clone()));
        Self {
            leaked,
            output,
            input,
            _raw: raw,
        }
    }
    /// Waits (bounded) for the last output of an ended terminal, restores the
    /// pane's mode and says whether a known secret was masked.
    pub async fn finish(mut self) -> bool {
        if tokio::time::timeout(Duration::from_secs(2), &mut self.output)
            .await
            .is_err()
        {
            // Wait for the cancelled task: the pane writer is released before we return.
            self.output.abort();
            let _ = (&mut self.output).await;
        }
        self.input.abort();
        self.leaked.load(Ordering::SeqCst)
    }
}

/// Attaches to a container's terminal over the daemon's Unix socket
/// (`POST <path>` with a connection upgrade). The returned stream is the raw
/// terminal: no byte of it is consumed here.
pub async fn attach_unix(socket: &Path, path: &str) -> Result<tokio::net::UnixStream> {
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: docker\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        ensure!(head.len() < 8192, "terminal attach response too long");
        head.push(stream.read_u8().await.context("terminal attach closed")?);
    }
    ensure!(
        head.starts_with(b"HTTP/1.1 101") || head.starts_with(b"HTTP/1.1 200"),
        "terminal attach refused: {}",
        String::from_utf8_lossy(&head).lines().next().unwrap_or("")
    );
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn secret(s: &str) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(s.as_bytes().to_vec())
    }
    #[test]
    fn a_known_secret_is_masked_at_every_split_and_other_bytes_are_not_delayed() {
        let sample = b"login eyJhbGciOi-secret-0123456789 done, eyJ is no secret\r\n";
        for at in 0..=sample.len() {
            let mut r =
                ExactRedactor::new(vec![secret("eyJhbGciOi-secret-0123456789"), secret("")]);
            let mut out = r.frame(&sample[..at]);
            // Nothing that could still become the secret has left yet.
            assert!(!String::from_utf8_lossy(&out).contains("eyJhbGciOi-secret-01234567"));
            out.extend(r.frame(&sample[at..]));
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "login **************************** done, eyJ is no secret\r\n",
                "split at {at}"
            );
            assert!(r.leaked);
        }
        let mut r = ExactRedactor::new(vec![secret("token-abc")]);
        assert_eq!(r.frame(b"plain x"), b"plain x");
        assert_eq!(r.frame(b"to"), b"", "a possible start is held");
        assert_eq!(r.frame(b"p"), b"top");
        assert!(!r.leaked);
    }
    #[tokio::test]
    async fn relay_carries_typed_bytes_in_and_redacted_output_back() {
        let (ours, mut sandbox) = tokio::io::duplex(1024);
        let (input, mut keys) = std::io::pipe().unwrap();
        let (mut screen, output) = std::io::pipe().unwrap();
        let pane = Pane {
            input: Box::new(input),
            output: Box::new(output),
            tty: false,
        };
        let relay = Relay::start(ours, pane, vec![secret("login-token-0123456789")]);
        keys.write_all(b"hi\r").unwrap();
        let mut typed = [0u8; 3];
        sandbox.read_exact(&mut typed).await.unwrap();
        assert_eq!(&typed, b"hi\r");
        let credential = format!("aqa_{}", "Q".repeat(43));
        let text = format!("a login-token-0123456789 b {credential}\n");
        sandbox.write_all(text.as_bytes()).await.unwrap();
        drop(sandbox);
        assert!(relay.finish().await, "the login token was seen");
        let mut shown = String::new();
        screen.read_to_string(&mut shown).unwrap();
        assert_eq!(shown.len(), text.len());
        assert!(shown.starts_with("a ********************** b aqa_***"));
        assert!(!shown.contains("QQQ"));
    }
}
