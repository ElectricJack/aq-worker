//! Route-agnostic AQ forwarding for sandboxes.
//!
//! - [`AgentChannel`]: the per-attempt agent channel of spec 0006 §5 (aq-server
//!   `docs/specs/0006-agent-channel.md`). A Unix socket, bind-mounted into the
//!   attempt's sandbox at [`CONTAINER_DIR`], that forwards HTTP to aq-server's
//!   machine listener without knowing any command. It holds the attempt's
//!   `aqa_` credential in memory, replaces any caller `Authorization`, refuses
//!   node and worker namespaces, rotates the credential before expiry and
//!   closes on stop, grant end or revocation. The agent holds no secret.
//! - [`LoopbackProxy`]: the B6b runner proxy (fixed upstream, Access headers
//!   injected, agent `Authorization` passed through), on the same core.
//! - [`holds_agent_credential`]: the leak guard's `aqa_` credential scan.
mod channel;
mod forward;
mod issuer;
mod loopback;
#[cfg(test)]
mod tests;

pub use channel::{AgentChannel, ChannelConfig, NodeLink, sweep};
pub use forward::{AGENT_PREFIXES, NODE_PREFIXES, agent_path, node_origin};
pub use issuer::{AttemptIdentity, CredentialIssuer, IssueError, IssueFuture, Issued, NodeIssuer};
pub use loopback::LoopbackProxy;

use anyhow::{Result, ensure};
use serde::Deserialize;
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Semaphore, watch},
    task::JoinSet,
};
use zeroize::Zeroizing;

/// Where the channel directory is mounted inside the sandbox.
pub const CONTAINER_DIR: &str = "/aq/channel";
/// The socket's file name inside the channel directory.
pub const SOCKET_NAME: &str = "aq.sock";
/// The `AQ_CHANNEL` value the CLI reads in channel mode (0006 §4.1). Not secret.
pub const AQ_CHANNEL: &str = "unix:/aq/channel/aq.sock";
/// Launch refusal code when a channel cannot be established (0006 §5.4).
pub const UNAVAILABLE: &str = "agent_channel_unavailable";
/// Prefix of every agent credential aq-server issues.
pub const CREDENTIAL_PREFIX: &str = "aqa_";
/// Shortest run of token characters after the prefix that counts as a
/// credential. aq-server issues 43 (32 random bytes, base64url); the bound
/// keeps AQ's own source text (`starts_with("aqa_")`) from tripping the guard.
pub const MIN_CREDENTIAL_TAIL: usize = 32;

/// Whether `bytes` holds an agent-credential-shaped string: `aqa_` followed by
/// at least [`MIN_CREDENTIAL_TAIL`] base64url characters. The guard needs no
/// copy of the credential, so it also runs where the credential never is.
pub fn holds_agent_credential(bytes: &[u8]) -> bool {
    holds_token_shape(bytes, CREDENTIAL_PREFIX, MIN_CREDENTIAL_TAIL)
}

/// True when `bytes` hold the Claude subscription token shape `sk-ant-[A-Za-z0-9_-]{20,}`.
/// Salvage after a restart no longer has the leased token, so it checks the shape.
pub fn holds_claude_token_shape(bytes: &[u8]) -> bool {
    holds_token_shape(bytes, "sk-ant-", 20)
}

fn holds_token_shape(bytes: &[u8], prefix: &str, min_tail: usize) -> bool {
    let prefix = prefix.as_bytes();
    let token = |b: &u8| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_';
    bytes.windows(prefix.len()).enumerate().any(|(at, w)| {
        w == prefix
            && bytes[at + prefix.len()..]
                .iter()
                .take_while(|b| token(b))
                .count()
                >= min_tail
    })
}

/// Where the sandbox template installs the `aq` CLI (0006 §9 step 3).
pub const AQ_BIN: &str = "/opt/aq/bin";

/// `PATH` for an agent with a channel: `/opt/aq/bin` first, once.
pub fn with_aq_bin(path: &str) -> String {
    if path.split(':').any(|p| p == AQ_BIN) {
        path.to_owned()
    } else {
        format!("{AQ_BIN}:{path}")
    }
}

/// The driver's launch gate (0006 §5.4): `<root>/<attempt>/aq.sock` must be
/// the node's live socket before the sandbox exists.
pub fn socket_ready(root: Option<&std::path::Path>, attempt: uuid::Uuid) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let ok = root
        .and_then(|r| std::fs::symlink_metadata(r.join(attempt.to_string()).join(SOCKET_NAME)).ok())
        .is_some_and(|m| m.file_type().is_socket());
    ensure!(ok, "{UNAVAILABLE}: no channel socket for this attempt");
    Ok(())
}

/// Whether `a` and `b` overlap: one equals, contains or sits under the other,
/// lexically or after resolving symlinks (a path that does not exist yet
/// resolves through its nearest existing ancestor).
pub fn paths_overlap(a: &std::path::Path, b: &std::path::Path) -> bool {
    fn resolved(path: &std::path::Path) -> std::path::PathBuf {
        let mut rest = Vec::new();
        let mut at = path;
        loop {
            if let Ok(real) = std::fs::canonicalize(at) {
                return rest.iter().rev().fold(real, |p, c| p.join(c));
            }
            match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    at = parent;
                }
                _ => return path.to_owned(),
            }
        }
    }
    let forms = |p: &std::path::Path| [p.to_owned(), resolved(p)];
    forms(a).iter().any(|x| {
        forms(b)
            .iter()
            .any(|y| x.starts_with(y) || y.starts_with(x))
    })
}

/// Cloudflare Access service-token headers for a production upstream.
#[derive(Clone)]
pub struct AccessHeaders {
    client_id: Zeroizing<String>,
    client_secret: Zeroizing<String>,
}
impl std::fmt::Debug for AccessHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessHeaders { <redacted> }")
    }
}
impl AccessHeaders {
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let headers = Self {
            client_id: Zeroizing::new(client_id),
            client_secret: Zeroizing::new(client_secret),
        };
        headers.validate()?;
        Ok(headers)
    }
    /// The B6b runner `access.json` shape: `{"client_id","client_secret"}`.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            client_id: String,
            client_secret: String,
        }
        let raw: Raw = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid runner Access file"))?;
        Self::new(raw.client_id, raw.client_secret)
    }
    fn validate(&self) -> Result<()> {
        for value in [&self.client_id, &self.client_secret] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 4096
                    && value.bytes().all(|b| b.is_ascii_graphic()),
                "invalid runner Access value"
            );
        }
        Ok(())
    }
    fn values(&self) -> [&[u8]; 2] {
        [self.client_id.as_bytes(), self.client_secret.as_bytes()]
    }
    fn headers(&self) -> Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>> {
        let mut out = Vec::new();
        for (name, value) in [
            ("cf-access-client-id", &self.client_id),
            ("cf-access-client-secret", &self.client_secret),
        ] {
            let mut value = reqwest::header::HeaderValue::from_str(value)?;
            value.set_sensitive(true);
            out.push((reqwest::header::HeaderName::from_static(name), value));
        }
        Ok(out)
    }
}

/// A listener the serving loop accepts from. `Ok(None)` drops one connection
/// (for example a non-loopback TCP peer); `Err` ends the loop.
pub(crate) trait Accept: Send + 'static {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    fn accept(
        &mut self,
    ) -> impl std::future::Future<Output = std::io::Result<Option<Self::Stream>>> + Send;
}

/// Bounded-concurrency accept loop. On stop it accepts nothing more, lets
/// in-flight requests finish for `grace`, then cancels the rest.
pub(crate) async fn serve<A, H, F>(
    mut listener: A,
    mut stopped: watch::Receiver<bool>,
    slots: usize,
    grace: std::time::Duration,
    handle: H,
) where
    A: Accept,
    H: Fn(A::Stream) -> F + Send + 'static,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(slots));
    let mut requests = JoinSet::new();
    if !*stopped.borrow_and_update() {
        loop {
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                Some(_) = requests.join_next(), if !requests.is_empty() => {},
                incoming = listener.accept() => {
                    let stream = match incoming {
                        Err(_) => break,
                        Ok(None) => continue,
                        Ok(Some(stream)) => stream,
                    };
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                    let request = handle(stream);
                    requests.spawn(async move {
                        let _slot = slot;
                        request.await;
                    });
                }
            }
        }
    }
    drop(listener);
    if !grace.is_zero() {
        let _ = tokio::time::timeout(grace, async {
            while requests.join_next().await.is_some() {}
        })
        .await;
    }
    // Dropping the JoinSet cancels every remaining connection.
}
