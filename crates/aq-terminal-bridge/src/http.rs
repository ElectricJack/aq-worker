//! Fixed outbound routes. No automatic mutation retry, redirect, ambient proxy,
//! inbound listener or network operation while a Bridge/guardian lock is held.
use anyhow::{Result, ensure};
use aq_session_protocol::{TerminalPolicy, Validate};
use aq_terminal_relay::api::{MutationReply, TerminalInfo}; // Decode-only frozen public DTOs.
use aq_terminal_relay_protocol::{
    self as wire, Envelope, InputMode, Peer, PeerUpload, Register, RelayPoll, UploadAck,
};
use reqwest::{Url, header};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

pub struct NodeCredential(Zeroizing<String>);
impl NodeCredential {
    pub fn new(value: String) -> Result<Self> {
        ensure!(
            !value.is_empty() && value.len() <= 512 && value.bytes().all(|b| b.is_ascii_graphic()),
            "invalid node credential"
        );
        Ok(Self(Zeroizing::new(value)))
    }
}
impl std::fmt::Debug for NodeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeCredential([REDACTED])")
    }
}
#[derive(Clone)]
pub struct RelayHttp {
    client: reqwest::Client,
    origin: Url,
    authorization: header::HeaderValue,
    access: Option<(header::HeaderValue, header::HeaderValue)>,
    poll: Arc<Semaphore>,
    output_poll: Arc<Semaphore>,
    control: Arc<Semaphore>,
    output: Arc<Semaphore>,
}
impl RelayHttp {
    pub fn new(origin: &str, credential: &NodeCredential) -> Result<Self> {
        Self::build(origin, credential, false)
    }
    /// Explicit trusted same-host node transport; never a provider endpoint.
    pub fn trusted_local_node(origin: &str, credential: &NodeCredential) -> Result<Self> {
        let selected =
            Url::parse(origin).map_err(|_| anyhow::anyhow!("invalid local node origin"))?;
        ensure!(
            selected.scheme() == "http"
                && selected.host_str() == Some("127.0.0.1")
                && selected.port().is_some_and(|p| p > 0),
            "local node requires explicit numeric loopback port"
        );
        Self::build(origin, credential, true)
    }
    #[cfg(feature = "test-support")]
    pub fn development_loopback(origin: &str, credential: &NodeCredential) -> Result<Self> {
        Self::build(origin, credential, true)
    }
    fn build(origin: &str, credential: &NodeCredential, development: bool) -> Result<Self> {
        let origin =
            Url::parse(origin).map_err(|_| anyhow::anyhow!("invalid terminal relay origin"))?;
        ensure!(
            origin.host_str().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "terminal relay requires credential-free origin"
        );
        ensure!(
            origin.scheme() == "https"
                || (development
                    && origin.scheme() == "http"
                    && origin.host_str().is_some_and(|h| h
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback()))),
            "terminal relay requires HTTPS; loopback fixture mode is explicit"
        );
        let value = Zeroizing::new(format!("Bearer {}", &*credential.0));
        let mut authorization = header::HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid node authorization"))?;
        authorization.set_sensitive(true);
        let client = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(22))
            .pool_max_idle_per_host(3)
            .build()
            .map_err(|_| anyhow::anyhow!("terminal relay HTTP configuration failed"))?;
        Ok(Self {
            client,
            origin,
            authorization,
            access: None,
            poll: Arc::new(Semaphore::new(1)),
            output_poll: Arc::new(Semaphore::new(1)),
            control: Arc::new(Semaphore::new(1)),
            output: Arc::new(Semaphore::new(1)),
        })
    }
    /// Adds Cloudflare Access service-token headers to every request.
    pub fn with_access(mut self, client_id: &str, client_secret: &str) -> Result<Self> {
        let value = |v: &str| {
            let mut h = header::HeaderValue::from_str(v)
                .map_err(|_| anyhow::anyhow!("invalid relay Access value"))?;
            h.set_sensitive(true);
            Ok::<_, anyhow::Error>(h)
        };
        self.access = Some((value(client_id)?, value(client_secret)?));
        Ok(self)
    }
    /// Caller owns the immutable registration operation ID and expected sealed
    /// policy. A lost response is returned as uncertainty, never auto-retried.
    pub async fn register(
        &self,
        request: &Register,
        expected_policy: &TerminalPolicy,
    ) -> Result<TerminalInfo> {
        request.validate()?;
        ensure!(
            matches!(request.peer, Peer::Remote { .. })
                && request.input_mode == InputMode::FiniteV1,
            "stage1 HTTP relay supports remote finite input only"
        );
        let _slot = self
            .control
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("terminal relay control lane busy"))?;
        let bytes = self
            .post(
                "/v1/terminal-relay/register",
                request,
                64 * 1024,
                Duration::from_secs(4),
            )
            .await?;
        let reply: MutationReply = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid terminal registration reply"))?;
        let MutationReply::Registered { operation_id, info } = reply else {
            anyhow::bail!("unexpected terminal registration reply");
        };
        info.terminal.validate()?;
        ensure!(
            operation_id == request.operation_id
                && info.terminal == request.terminal
                && info.session_revision == request.expected_authority_revision
                && info.input_mode == InputMode::FiniteV1
                && info.terminal_policy == *expected_policy
                && info.control.terminal == request.terminal
                && info.control.session_revision == info.session_revision,
            "terminal registration reply binding mismatch"
        );
        if let Some(lease) = &info.control.lease {
            lease.validate()?;
            ensure!(
                lease.terminal == request.terminal && lease.generation == info.control.generation,
                "terminal registration controller mismatch"
            );
        }
        Ok(info)
    }
    pub async fn poll(&self, request: &RelayPoll) -> Result<Envelope> {
        request.validate()?;
        ensure!(
            matches!(request.peer, Peer::Remote { .. })
                && request.capability.as_str() == wire::CAPABILITY,
            "stage1 HTTP relay peer/capability unsupported"
        );
        let _slot = self
            .poll
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("terminal relay poll lane busy"))?;
        let bytes = self
            .post(
                "/v1/terminal-relay/poll",
                request,
                aq_session_protocol::MAX_BATCH_BYTES,
                Duration::from_millis(request.hold_ms as u64 + 2000),
            )
            .await?;
        let reply = wire::decode(&bytes)?;
        ensure!(
            reply.peer == request.peer
                && reply.capability == request.capability
                && reply.cursor >= request.cursor,
            "terminal poll reply binding mismatch"
        );
        Ok(reply)
    }
    /// Explicit Watch-only transport. This is not full polling with a controller
    /// hint: it cannot receive or acknowledge input/control/capture/fence work.
    pub async fn output_poll(&self, request: &RelayPoll) -> Result<Envelope> {
        request.validate()?;
        ensure!(
            matches!(request.peer, Peer::Remote { .. })
                && request.capability.as_str() == wire::CAPABILITY
                && request.hold_ms == 0
                && request.active_controllers == 0,
            "output poll requires remote nonblocking Watch-only profile"
        );
        let _slot = self
            .output_poll
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("terminal relay output poll lane busy"))?;
        let bytes = self
            .post(
                "/v1/terminal-relay/output-poll",
                request,
                aq_session_protocol::MAX_BATCH_BYTES,
                Duration::from_secs(2),
            )
            .await?;
        let reply = wire::decode(&bytes)?;
        ensure!(
            reply.peer == request.peer
                && reply.capability == request.capability
                && reply.cursor >= request.cursor
                && reply
                    .effects
                    .iter()
                    .all(|e| matches!(e, wire::Effect::Watch(_))),
            "output poll reply binding or Watch-only profile mismatch"
        );
        Ok(reply)
    }
    pub async fn control_upload(&self, request: &PeerUpload) -> Result<UploadAck> {
        request.validate()?;
        ensure!(
            matches!(request.peer, Peer::Remote { .. })
                && request.capability.as_str() == wire::CAPABILITY
                && request.frames.is_empty()
                && request.snapshots.is_empty()
                && request.gaps.is_empty()
                && request.epoch_receipts.is_empty(),
            "stage1 control upload contains unsupported content"
        );
        ensure!(
            serde_json::to_vec(request)?.len() <= 256 * 1024,
            "terminal control upload bytes"
        );
        let _slot = self
            .control
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("terminal relay control lane busy"))?;
        let bytes = self
            .post(
                "/v1/terminal-relay/control-upload",
                request,
                256 * 1024,
                Duration::from_secs(4),
            )
            .await?;
        let reply: UploadAck = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid terminal upload reply"))?;
        reply.validate()?;
        ensure!(
            reply.peer == request.peer
                && reply.exchange_id == request.exchange_id
                && reply.cursor == request.cursor
                && reply.capability == request.capability
                && reply.retirements.is_empty()
                && reply.dropped_output.is_empty(),
            "terminal upload reply binding mismatch"
        );
        Ok(reply)
    }
    /// Volatile output only, on its own lane. Snapshot producers must qualify
    /// their renderer/baseline; this transport validates only the opaque wire.
    /// Receipts stay on control.
    pub async fn output_upload(&self, request: &PeerUpload) -> Result<UploadAck> {
        request.validate()?;
        ensure!(
            matches!(request.peer, Peer::Remote { .. })
                && request.capability.as_str() == wire::CAPABILITY
                && request.receipts.is_empty()
                && request.epoch_receipts.is_empty()
                && request.resize_receipts.is_empty()
                && request.fence_acks.is_empty()
                && (!request.frames.is_empty()
                    || !request.gaps.is_empty()
                    || !request.snapshots.is_empty()),
            "tail upload contains unsupported content"
        );
        let _slot = self
            .output
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("terminal relay output lane busy"))?;
        let bytes = self
            .post(
                "/v1/terminal-relay/upload",
                request,
                256 * 1024,
                Duration::from_secs(4),
            )
            .await?;
        let reply: UploadAck = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid output upload reply"))?;
        reply.validate()?;
        ensure!(
            reply.peer == request.peer
                && reply.exchange_id == request.exchange_id
                && reply.cursor == request.cursor
                && reply.capability == request.capability
                && reply.retirements.is_empty(),
            "output upload reply binding mismatch"
        );
        ensure!(
            reply.dropped_output.iter().all(|gap| request
                .frames
                .iter()
                .any(|frame| frame.terminal == gap.terminal)),
            "foreign output drop acknowledgement"
        );
        Ok(reply)
    }
    async fn post(
        &self,
        path: &str,
        body: &impl Serialize,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(body)
            .map_err(|_| anyhow::anyhow!("invalid terminal request encoding"))?;
        ensure!(
            bytes.len() <= aq_session_protocol::MAX_BATCH_BYTES,
            "terminal request bytes"
        );
        let url = self
            .origin
            .join(path)
            .map_err(|_| anyhow::anyhow!("terminal relay route invalid"))?;
        let mut request = self
            .client
            .post(url)
            .header(header::AUTHORIZATION, self.authorization.clone());
        if let Some((id, secret)) = &self.access {
            request = request
                .header("cf-access-client-id", id.clone())
                .header("cf-access-client-secret", secret.clone());
        }
        let mut response = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("terminal relay response unknown"))?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "terminal relay request refused"
        );
        ensure!(
            response.content_length().is_none_or(|v| v <= limit as u64)
                && response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v
                        .split(';')
                        .next()
                        .is_some_and(|v| v.trim() == "application/json")),
            "terminal relay response headers/size invalid"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("terminal relay response unknown"))?
        {
            ensure!(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                "terminal relay response bytes"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}
