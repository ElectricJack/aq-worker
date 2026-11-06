//! The per-attempt agent channel (spec 0006 §5.1, docker-local placement).
//!
//! The node creates `<channel root>/<attempt>/aq.sock` and the sandbox driver
//! bind-mounts the directory (never the socket file) at `/aq/channel`. For
//! each connection the channel applies the B6b framing rules, refuses paths
//! outside the agent prefixes with `channel_refused`, removes any caller
//! `Authorization` and `cf-access-*` headers, injects the attempt credential
//! (and Access headers when configured) and forwards to the fixed origin.
//!
//! Lifetime (0006 §5.4): the credential is issued before the socket exists,
//! so a channel that opens is usable; it rotates before expiry and never past
//! `not_after_ms`; it closes on [`AgentChannel::close`], at `not_after_ms`, on
//! a terminal issuance refusal and when the server answers `session_revoked`.
//! Closing removes the socket directory, so later calls fail at connect
//! (`channel_closed` in the CLI). Logs carry method, path, status and
//! duration, never headers or bodies.
use crate::{
    Accept, AccessHeaders, CredentialIssuer, IssueError, Issued, SOCKET_NAME, UNAVAILABLE,
    forward::{Failure, Inject, Origins, Rules, Seen, Upstream, forward},
};
use anyhow::{Context, Result, ensure};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Upper bound on one forwarded exchange; `question wait` style reads are
/// bounded by the server well inside it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// How long in-flight calls may finish after a close.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Retry spacing after an unavailable rotation.
const RETRY: Duration = Duration::from_secs(5);

pub struct ChannelConfig {
    pub attempt_id: Uuid,
    /// The attempt's own directory, `<channel root>/<attempt>`. Its parent
    /// must exist; it is created (mode 0711) or reused after a node restart.
    pub directory: PathBuf,
    /// The node's configured aq-server origin.
    pub origin: String,
    /// Access headers for a production upstream; never for the local server.
    pub access: Option<AccessHeaders>,
    /// Unix ms: the earlier of the grant expiry and the hard deadline. No
    /// credential is used past it.
    pub not_after_ms: u64,
}

struct Credential {
    token: Zeroizing<String>,
    version: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

struct Shared {
    attempt_id: Uuid,
    upstream: Upstream,
    access: Option<AccessHeaders>,
    credential: RwLock<Credential>,
    closed: watch::Sender<bool>,
    not_after_ms: u64,
}
impl Shared {
    fn close(&self, why: &'static str) {
        if !self.closed.send_replace(true) {
            tracing::info!(target: "aq_agent_channel", attempt_id = %self.attempt_id, reason = why, "agent channel closed");
        }
    }
    fn inject(&self) -> Result<(Inject, Zeroizing<String>)> {
        let token = {
            let credential = self
                .credential
                .read()
                .map_err(|_| anyhow::anyhow!("credential lock"))?;
            credential.token.clone()
        };
        let bearer = Zeroizing::new(format!("Bearer {}", token.as_str()));
        let mut value = HeaderValue::from_str(&bearer)?;
        value.set_sensitive(true);
        let mut headers = vec![(AUTHORIZATION, value)];
        if let Some(access) = &self.access {
            headers.extend(access.headers()?);
        }
        Ok((headers, token))
    }
}

struct Socket(UnixListener);
impl Accept for Socket {
    type Stream = UnixStream;
    async fn accept(&mut self) -> std::io::Result<Option<UnixStream>> {
        Ok(Some(self.0.accept().await?.0))
    }
}

pub struct AgentChannel {
    shared: Arc<Shared>,
    directory: PathBuf,
    serve: Option<JoinHandle<()>>,
    rotate: Option<JoinHandle<()>>,
    _lock: std::fs::File,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn accepted(issued: &Issued, held: u64, now: u64) -> bool {
    issued.valid_shape() && issued.version > held && issued.expires_at_ms > now
}

/// Create (0711) or reuse the attempt directory, removing a stale socket.
fn prepare(directory: &Path) -> Result<()> {
    let parent = directory.parent().context("channel directory parent")?;
    let meta = std::fs::symlink_metadata(parent).context("channel root absent")?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0,
        "channel root must be a directory owned by the node user and not group/world writable"
    );
    match std::fs::symlink_metadata(directory) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
                "channel directory is not the node's own directory"
            );
            let socket = directory.join(SOCKET_NAME);
            if let Ok(old) = std::fs::symlink_metadata(&socket) {
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    old.file_type().is_socket(),
                    "channel directory holds a non-socket aq.sock"
                );
                std::fs::remove_file(&socket)?;
            }
        }
        Err(_) => std::fs::DirBuilder::new().mode(0o711).create(directory)?,
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o711))?;
    Ok(())
}

fn open_dir(directory: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)
}

/// Exclusive, non-blocking `flock` on the directory: held for the channel's
/// life, it marks the directory live; it is released when the node dies.
fn try_lock(dir: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Bind `<directory>/aq.sock` through the directory's descriptor, so a long
/// node state path never meets the 108-byte `sun_path` bound and the bind
/// cannot follow a swapped-in symlink. Returns the locked directory.
fn bind(directory: &Path) -> Result<(UnixListener, std::fs::File)> {
    use std::os::fd::AsRawFd;
    let dir = open_dir(directory)?;
    ensure!(
        try_lock(&dir),
        "another channel owns this attempt directory"
    );
    let path = format!("/proc/self/fd/{}/{SOCKET_NAME}", dir.as_raw_fd());
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    // The sandbox's agent user is unmapped (rootless) or another uid: it
    // reaches the socket as "other". Only this attempt's mount exposes it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    Ok((UnixListener::from_std(listener)?, dir))
}

impl AgentChannel {
    /// Issue the credential, then create the socket and start serving. Any
    /// failure is `agent_channel_unavailable`: the launch must not proceed.
    pub async fn open(config: ChannelConfig, issuer: Arc<dyn CredentialIssuer>) -> Result<Self> {
        Self::open_inner(config, issuer)
            .await
            .map_err(|error| anyhow::anyhow!("{UNAVAILABLE}: {error:#}"))
    }
    async fn open_inner(config: ChannelConfig, issuer: Arc<dyn CredentialIssuer>) -> Result<Self> {
        let upstream = Upstream::new(&config.origin, Origins::Either, REQUEST_TIMEOUT)?;
        let now = now_ms();
        ensure!(config.not_after_ms > now, "the attempt grant has ended");
        let issued = issuer.issue(0).await?;
        ensure!(
            accepted(&issued, 0, now),
            "issued credential shape, version or expiry refused"
        );
        prepare(&config.directory)?;
        let (listener, lock) = bind(&config.directory)?;
        let shared = Arc::new(Shared {
            attempt_id: config.attempt_id,
            upstream,
            access: config.access,
            credential: RwLock::new(Credential {
                token: issued.token,
                version: issued.version,
                issued_at_ms: now,
                expires_at_ms: issued.expires_at_ms,
            }),
            closed: watch::channel(false).0,
            not_after_ms: config.not_after_ms,
        });
        let serving = shared.clone();
        let directory = config.directory.clone();
        let serve = tokio::spawn(async move {
            let closed = serving.closed.subscribe();
            crate::serve(Socket(listener), closed, 16, CLOSE_GRACE, move |stream| {
                handle(serving.clone(), stream)
            })
            .await;
            // However the channel closed, later calls fail at connect.
            let _ = remove(&directory);
        });
        let rotate = tokio::spawn(rotate(shared.clone(), issuer));
        tracing::info!(target: "aq_agent_channel", attempt_id = %config.attempt_id, "agent channel open");
        Ok(Self {
            shared,
            directory: config.directory,
            serve: Some(serve),
            rotate: Some(rotate),
            _lock: lock,
        })
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn socket_path(&self) -> PathBuf {
        self.directory.join(SOCKET_NAME)
    }
    pub fn credential_version(&self) -> u64 {
        self.shared.credential.read().map_or(0, |c| c.version)
    }
    pub fn is_closed(&self) -> bool {
        *self.shared.closed.borrow()
    }
    /// Resolves once the channel has closed for any reason.
    pub fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut closed = self.shared.closed.subscribe();
        async move {
            let _ = closed.wait_for(|c| *c).await;
        }
    }
    /// Stop accepting (cancel, closure, revocation). Never blocks.
    pub fn close(&self) {
        self.shared.close("stopped by the node");
    }
    /// Close, wait for the serving tasks and remove the socket directory.
    pub async fn stop(mut self) -> Result<()> {
        self.close();
        if let Some(rotate) = self.rotate.take() {
            rotate.abort();
        }
        if let Some(serve) = self.serve.take() {
            let _ = serve.await;
        }
        remove(&self.directory)
    }
}
impl Drop for AgentChannel {
    fn drop(&mut self) {
        self.shared.close("dropped");
        for task in [self.rotate.take(), self.serve.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        let _ = remove(&self.directory);
    }
}

fn remove(directory: &Path) -> Result<()> {
    match std::fs::remove_dir_all(directory) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

async fn handle(shared: Arc<Shared>, mut stream: UnixStream) {
    let started = Instant::now();
    let mut seen = Seen::default();
    let rules = Rules {
        host: None,
        replace_authorization: true,
        agent_paths: true,
    };
    let outcome = match shared.inject() {
        Err(_) => Err(Failure::Upstream),
        Ok((inject, token)) => {
            let mut protected: Vec<&[u8]> = vec![token.as_bytes()];
            if let Some(access) = &shared.access {
                protected.extend(access.values());
            }
            tokio::time::timeout(
                REQUEST_TIMEOUT + Duration::from_secs(5),
                forward(
                    &shared.upstream,
                    &rules,
                    &inject,
                    &protected,
                    &mut stream,
                    &mut seen,
                ),
            )
            .await
            .unwrap_or(Err(Failure::Upstream))
        }
    };
    let (status, response, revoked) = match outcome {
        Ok(done) => (done.status, done.response, done.revoked),
        Err(failure) => {
            let (status, code, message, effect) = match failure {
                Failure::Refused(why) => (403, "channel_refused", why, "not_applied"),
                Failure::Framing => (
                    400,
                    "channel_refused",
                    "the request framing was refused",
                    "not_applied",
                ),
                Failure::Upstream => (
                    502,
                    "channel_upstream_failed",
                    "aq-server did not answer through the channel; for a mutation, check the operation before retrying",
                    "unknown",
                ),
            };
            // A refusal is a commands-1 `Refusal` (spec 0006 §6.8): the CLI
            // reads only that shape. An upstream failure is not a refusal.
            let mut body = serde_json::json!({"code": code, "message": message, "effect": effect});
            if effect == "not_applied" {
                body["command"] = serde_json::json!(command_name(&seen.path));
                body["grant"] = serde_json::Value::Null;
                body["held"] = serde_json::json!([]);
                body["request_id"] = serde_json::Value::Null;
            }
            let body = body.to_string();
            let head = format!(
                "HTTP/1.1 {status} Channel\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            (
                status,
                [head.into_bytes(), body.into_bytes()].concat(),
                false,
            )
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&response)).await;
    let _ = stream.shutdown().await;
    // Drain what a refused caller still sends, so closing with unread input
    // does not reset the connection before it reads the refusal.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        let mut sink = [0u8; 8192];
        let mut left = crate::forward::MAX_BODY;
        while left > 0 {
            match tokio::io::AsyncReadExt::read(&mut stream, &mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    })
    .await;
    tracing::info!(target: "aq_agent_channel", attempt_id = %shared.attempt_id,
        method = %seen.method, path = %seen.path, status,
        duration_ms = started.elapsed().as_millis() as u64, "agent channel request");
    if revoked {
        // The server has revoked the verifier (cancel, closure, disposition).
        shared.close("the server revoked the attempt credential");
    }
}

/// The command a refused request named: the last path segment under the
/// commands prefix, else the path itself (bounded; never empty).
fn command_name(path: &str) -> String {
    let name = path
        .strip_prefix("/api/v1/commands/")
        .filter(|n| !n.is_empty() && !n.contains('/'))
        .unwrap_or(path);
    let name: String = name.chars().take(128).collect();
    if name.is_empty() {
        "channel".into()
    } else {
        name
    }
}

/// Rotate at two thirds of each credential's life; close at `not_after_ms`
/// or on a terminal refusal. Issuance runs under the node credential, so a
/// rotation that runs late (a stalled node) still renews: calls in the gap
/// fail with the server's `session_expired`, and the channel stays usable.
async fn rotate(shared: Arc<Shared>, issuer: Arc<dyn CredentialIssuer>) {
    let mut closed = shared.closed.subscribe();
    let mut retry_at: Option<u64> = None;
    loop {
        let (version, due) = {
            let Ok(c) = shared.credential.read() else {
                return shared.close("credential lock");
            };
            // A credential that already reaches the grant end is never renewed.
            let due = if c.expires_at_ms >= shared.not_after_ms {
                shared.not_after_ms
            } else {
                retry_at.unwrap_or(c.issued_at_ms + (c.expires_at_ms - c.issued_at_ms) * 2 / 3)
            };
            (c.version, due.min(shared.not_after_ms))
        };
        let wait = Duration::from_millis(due.saturating_sub(now_ms()));
        tokio::select! {
            _ = closed.wait_for(|c| *c) => return,
            _ = tokio::time::sleep(wait) => {}
        }
        let now = now_ms();
        if now >= shared.not_after_ms {
            return shared.close("the attempt grant ended");
        }
        match issuer.issue(version).await {
            Ok(issued) if accepted(&issued, version, now) => {
                if let Ok(mut c) = shared.credential.write() {
                    *c = Credential {
                        token: issued.token,
                        version: issued.version,
                        issued_at_ms: now,
                        expires_at_ms: issued.expires_at_ms,
                    };
                }
                retry_at = None;
                tracing::info!(target: "aq_agent_channel", attempt_id = %shared.attempt_id,
                    version = issued.version, "agent credential rotated");
            }
            Ok(_) | Err(IssueError::Refused(_)) => {
                return shared.close("credential rotation was refused");
            }
            Err(IssueError::Unavailable) => {
                retry_at = Some(now + RETRY.as_millis() as u64);
            }
        }
    }
}

/// Removes the stale attempt directories under `root`: those no live channel
/// holds locked (their node process is gone; today `reconcile-native` stops
/// those sandboxes). Live channels and foreign entries are left alone.
/// Sandbox readoption must keep the directories of readopted attempts
/// instead; [`AgentChannel::open`] already rebinds inside an existing one.
pub fn sweep(root: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry?.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| Uuid::parse_str(n).is_ok());
        let Ok(dir) = open_dir(&path) else { continue };
        if named && try_lock(&dir) {
            remove(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}
