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
//! A 401 `unauthorized` or `session_expired` makes the node re-issue at once
//! (at most once per credential version, spaced by the retry interval): the
//! server's revocation and closure paths drop the verifier and answer
//! `unauthorized`, and that issuance answers `session_revoked`, which closes.
//! Closing removes the socket directory, so later calls fail at connect
//! (`channel_closed` in the CLI). Logs carry method, path, status and
//! duration, never headers or bodies.
use crate::{
    Accept, AccessHeaders, AttemptIdentity, CredentialIssuer, IssueError, Issued, NodeIssuer,
    SOCKET_NAME, UNAVAILABLE,
    forward::{Credentials, Failure, Origins, Rules, Seen, Taken, Upstream, forward},
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Notify, watch},
    task::JoinHandle,
};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Upper bound on one forwarded exchange; `question wait` style reads are
/// bounded by the server well inside it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// How long in-flight calls may finish after a close.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Retry spacing after an unavailable rotation, and the first spacing of
/// the re-issuance an upstream 401 asks for.
const RETRY: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(5)
};
/// 401-driven re-issues back off exponentially from [`RETRY`] up to this
/// multiple of it (320 s in production), until a scheduled rotation.
const STALE_BACKOFF_MAX: u32 = 64;

/// The newest credential version an upstream 401 was answered for. Versions
/// only grow, so a late 401 for an older version never hides a newer one.
#[derive(Default)]
pub(crate) struct StaleSignal {
    version: AtomicU64,
    notify: Notify,
}
impl StaleSignal {
    pub(crate) fn raise(&self, version: u64) {
        self.version.fetch_max(version, Ordering::SeqCst);
        self.notify.notify_one();
    }
    /// Whether a 401 has been answered for `held`, the version held now.
    pub(crate) fn raised_for(&self, held: u64) -> bool {
        self.version.load(Ordering::SeqCst) == held
    }
}

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
    /// An upstream 401 for a credential version: re-issue now if that is
    /// still the held version (bounded).
    stale: StaleSignal,
    not_after_ms: u64,
}
impl Credentials for Shared {
    fn take(&self) -> Option<Taken> {
        let credential = self.credential.read().ok()?;
        let bearer = Zeroizing::new(format!("Bearer {}", credential.token.as_str()));
        let mut value = HeaderValue::from_str(&bearer).ok()?;
        value.set_sensitive(true);
        let mut taken = Taken {
            headers: vec![(AUTHORIZATION, value)],
            protected: vec![Zeroizing::new(credential.token.as_bytes().to_vec())],
            version: Some(credential.version),
        };
        if let Some(access) = &self.access {
            taken.headers.extend(access.headers().ok()?);
            let values = access.values().map(|v| Zeroizing::new(v.to_vec()));
            taken.protected.extend(values);
        }
        Some(taken)
    }
    fn version(&self) -> Option<u64> {
        self.credential.read().ok().map(|c| c.version)
    }
}
impl Shared {
    fn close(&self, why: &'static str) {
        if !self.closed.send_replace(true) {
            tracing::info!(target: "aq_agent_channel", attempt_id = %self.attempt_id, reason = why, "agent channel closed");
        }
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

pub(crate) fn open_dir(directory: &Path) -> std::io::Result<std::fs::File> {
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

/// Create (0711) or reuse the attempt directory, lock it, then bind
/// `aq.sock` in it through the directory's descriptor
/// (`/proc/self/fd/N/aq.sock`), so a long node state path never meets the
/// 108-byte `sun_path` bound and the bind cannot follow a swapped-in symlink.
///
/// [`sweep`] removes only a directory it holds locked, so it may remove one
/// this call has created but not yet locked. After locking, the directory
/// must still be the one named `directory`, or the claim starts over. A
/// sweep holds its lock only while it removes; a live channel holds it for
/// good, and then the claim fails. Returns the locked directory.
async fn claim(directory: &Path) -> Result<(UnixListener, std::fs::File)> {
    use std::os::{fd::AsRawFd, unix::fs::FileTypeExt};
    let euid = unsafe { libc::geteuid() };
    let parent = directory.parent().context("channel directory parent")?;
    let meta = std::fs::symlink_metadata(parent).context("channel root absent")?;
    ensure!(
        meta.is_dir() && meta.uid() == euid && meta.mode() & 0o022 == 0,
        "channel root must be a directory owned by the node user and not group/world writable"
    );
    for _ in 0..50 {
        match std::fs::DirBuilder::new().mode(0o711).create(directory) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error.into());
            }
            _ => {}
        }
        let dir = match open_dir(directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            other => other.context("channel directory is not the node's own directory")?,
        };
        if !try_lock(&dir) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        if !is_at(directory, &dir) {
            continue; // swept between creation and lock
        }
        let held = dir.metadata()?;
        ensure!(
            held.is_dir() && held.uid() == euid,
            "channel directory is not the node's own directory"
        );
        ensure!(
            unsafe { libc::fchmod(dir.as_raw_fd(), 0o711) } == 0,
            "channel directory mode"
        );
        let path = format!("/proc/self/fd/{}/{SOCKET_NAME}", dir.as_raw_fd());
        if let Ok(old) = std::fs::symlink_metadata(&path) {
            ensure!(
                old.file_type().is_socket(),
                "channel directory holds a non-socket aq.sock"
            );
            std::fs::remove_file(&path)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        // The sandbox's agent user is unmapped (rootless) or another uid: it
        // reaches the socket as "other". Only this attempt's mount exposes it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        return Ok((UnixListener::from_std(listener)?, dir));
    }
    bail!("another channel owns this attempt directory")
}

/// The node's own aq-server connection as its enrollment (`worker.toml`)
/// gives it: `server_url`, the node credential and the Cloudflare Access
/// service token. Issuance and every forwarded request use all three, so the
/// channel reaches exactly what the node itself reaches. Memory only.
#[derive(Clone)]
pub struct NodeLink {
    origin: String,
    node_credential: Zeroizing<String>,
    access: Option<AccessHeaders>,
}
impl std::fmt::Debug for NodeLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeLink {{ origin: {:?}, <redacted> }}", self.origin)
    }
}
impl NodeLink {
    pub fn new(
        server_url: &str,
        node_credential: &str,
        cf_client_id: Option<&str>,
        cf_client_secret: Option<&str>,
    ) -> Result<Self> {
        crate::node_origin(server_url)?;
        let access = match (cf_client_id, cf_client_secret) {
            (None, None) => None,
            (Some(id), Some(secret)) => Some(AccessHeaders::new(id.into(), secret.into())?),
            _ => bail!("incomplete machine authentication configuration"),
        };
        Ok(Self {
            origin: server_url.into(),
            node_credential: Zeroizing::new(node_credential.into()),
            access,
        })
    }
    /// Issue the attempt's credential and open its channel in `directory`.
    pub async fn open(
        &self,
        attempt: AttemptIdentity,
        directory: PathBuf,
        not_after_ms: u64,
    ) -> Result<AgentChannel> {
        let issuer = NodeIssuer::new(
            &self.origin,
            &self.node_credential,
            self.access.clone(),
            attempt,
        )
        .map_err(|error| anyhow::anyhow!("{UNAVAILABLE}: {error:#}"))?;
        let config = ChannelConfig {
            attempt_id: attempt.attempt_id,
            directory,
            origin: self.origin.clone(),
            access: self.access.clone(),
            not_after_ms,
        };
        AgentChannel::open(config, Arc::new(issuer)).await
    }
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
        let upstream = Upstream::new(&config.origin, Origins::Node, REQUEST_TIMEOUT)?;
        let now = now_ms();
        ensure!(config.not_after_ms > now, "the attempt grant has ended");
        let issued = issuer.issue(0).await?;
        ensure!(
            accepted(&issued, 0, now),
            "issued credential shape, version or expiry refused"
        );
        let (listener, lock) = claim(&config.directory).await?;
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
            stale: StaleSignal::default(),
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
    let outcome = tokio::time::timeout(
        REQUEST_TIMEOUT + Duration::from_secs(5),
        forward(&shared.upstream, &rules, &*shared, &mut stream, &mut seen),
    )
    .await
    .unwrap_or(Err(Failure::Upstream));
    let (status, response, refused, version, foreign_version) = match outcome {
        Ok(done) => (
            done.status,
            done.response,
            done.refused,
            done.version,
            done.foreign_version,
        ),
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
                None,
                None,
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
    match refused.as_deref() {
        // The server has revoked the verifier (cancel, closure, disposition).
        Some("session_revoked") => shared.close("the server revoked the attempt credential"),
        // The held credential no longer authenticates: revoked (the server
        // drops the verifier), or a rotation response was lost. Re-issuing
        // tells which; the rotation task bounds it.
        // A caller-chosen API version gets `unauthorized` from the command
        // surface whatever the credential; it never asks for a re-issue.
        Some("unauthorized" | "session_expired") if !foreign_version => {
            shared.stale.raise(version.unwrap_or(0));
        }
        _ => {}
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
///
/// - **Stale credential.** An upstream 401 for the held version re-issues at
///   once, at most once per held version. The spacing from the previous
///   issuance starts at [`RETRY`] and doubles with each such re-issue up to
///   [`STALE_BACKOFF_MAX`] times it; a scheduled rotation resets it. A
///   sandbox that provokes 401s therefore forces only a few rotations per
///   rotation period. A revoked or closed attempt answers the re-issue with
///   `session_revoked`, and the channel closes.
/// - **Lost response.** The server commits a rotation before it answers, and
///   keeps no idempotency record. If an issuance came back unavailable, the
///   server may hold `expected + 1` and answer the retry `id_conflict`. Only
///   then is the next issuance fenced on `expected + 1`. An `id_conflict`
///   that no unavailable issuance precedes is the fence doing its job: close.
async fn rotate(shared: Arc<Shared>, issuer: Arc<dyn CredentialIssuer>) {
    let mut closed = shared.closed.subscribe();
    let mut retry_at: Option<u64> = None;
    // Server versions committed whose responses were lost.
    let mut lost = 0u64;
    let mut uncertain = false;
    let mut stale = false;
    let mut probed: Option<u64> = None;
    let mut last_issue = 0u64;
    let base = RETRY.as_millis() as u64;
    let mut spacing = base;
    loop {
        let (version, due) = {
            let Ok(c) = shared.credential.read() else {
                return shared.close("credential lock");
            };
            // A credential that already reaches the grant end is never renewed.
            let mut due = if c.expires_at_ms >= shared.not_after_ms {
                shared.not_after_ms
            } else {
                retry_at.unwrap_or(c.issued_at_ms + (c.expires_at_ms - c.issued_at_ms) * 2 / 3)
            };
            if stale {
                due = due.min(last_issue + spacing);
            }
            (c.version, due.min(shared.not_after_ms))
        };
        let wait = Duration::from_millis(due.saturating_sub(now_ms()));
        tokio::select! {
            _ = closed.wait_for(|c| *c) => return,
            _ = tokio::time::sleep(wait) => {}
            _ = shared.stale.notify.notified(), if !stale => {
                stale = probed != Some(version) && shared.stale.raised_for(version);
                continue;
            }
        }
        let now = now_ms();
        if now >= shared.not_after_ms {
            return shared.close("the attempt grant ended");
        }
        let reissue = std::mem::take(&mut stale);
        if reissue {
            probed = Some(version);
            spacing = (spacing * 2).min(base * u64::from(STALE_BACKOFF_MAX));
        }
        last_issue = now;
        let expected = version + lost;
        match issuer.issue(expected).await {
            Ok(issued) if accepted(&issued, expected, now) => {
                if let Ok(mut c) = shared.credential.write() {
                    *c = Credential {
                        token: issued.token,
                        version: issued.version,
                        issued_at_ms: now,
                        expires_at_ms: issued.expires_at_ms,
                    };
                }
                (retry_at, lost, uncertain) = (None, 0, false);
                if !reissue {
                    spacing = base;
                }
                tracing::info!(target: "aq_agent_channel", attempt_id = %shared.attempt_id,
                    version = issued.version, "agent credential rotated");
            }
            Err(IssueError::Refused(code)) if code == "id_conflict" && uncertain => {
                // Our unanswered issuance was committed: fence on its version.
                (retry_at, lost, uncertain) = (Some(now), lost + 1, false);
            }
            Ok(_) | Err(IssueError::Refused(_)) => {
                return shared.close("credential rotation was refused");
            }
            Err(IssueError::Unavailable) => {
                retry_at = Some(now + RETRY.as_millis() as u64);
                uncertain = true;
            }
        }
    }
}

/// Removes the stale attempt directories under `root`: those no live channel
/// holds locked (their node process is gone; today `reconcile-native` stops
/// those sandboxes). Live channels and foreign entries are left alone.
/// Sandbox readoption must keep the directories of readopted attempts
/// instead; [`AgentChannel::open`] already rebinds inside an existing one.
/// A directory an open is still creating may be removed; the open's claim
/// notices and creates it again.
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
        if named && sweep_one(&path, &dir)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove `path` if `dir` (opened from it) is unlocked and still the
/// directory at `path`. Another sweep may have removed it since it was opened
/// and a claim created and locked a new one there; locking the old handle
/// then succeeds, but the path is no longer its directory.
pub(crate) fn sweep_one(path: &Path, dir: &std::fs::File) -> Result<bool> {
    if !try_lock(dir) || !is_at(path, dir) {
        return Ok(false);
    }
    remove(path)?;
    Ok(true)
}

/// Whether the directory `dir` is the one named `path` (same device and inode).
fn is_at(path: &Path, dir: &std::fs::File) -> bool {
    let (Ok(held), Ok(named)) = (dir.metadata(), std::fs::symlink_metadata(path)) else {
        return false;
    };
    named.dev() == held.dev() && named.ino() == held.ino()
}
