//! Permit-only outbound runner boundary. This is separate from node clients:
//! construction never accepts a WorkerConfig or node/Cloudflare credential.
use super::{FrozenLaunch, private::RunnerFiles};
use anyhow::{Result, ensure};
use aq_admission_sandbox::{control::DeliveryStamp, sandbox::*};
use aq_session_protocol::{LaunchBinding, SessionIdentity};
use aq_worker_protocol::{TransferFile, TransferManifest};
use reqwest::{StatusCode, Url, header};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const MAX_BODY: usize = 4 * 1024 * 1024;
#[derive(Clone)]
pub enum RunnerAuthority {
    Preparation(PreparationAuthority),
    Execution(PhaseExecutionAuthority),
}
/// Original authenticated delivery observations. Reading a file never restarts
/// its TTL. The kernel boot and server-issued node boot are separate identities.
#[derive(Clone)]
pub struct RunnerHandoff {
    pub authority: RunnerAuthority,
    pub binding: PermitBinding,
    pub delivery: DeliveryStamp,
    pub sent_boot_ns: u64,
    pub received_boot_ns: u64,
    pub kernel_boot_id: String,
}
#[derive(Clone, Copy)]
pub enum TerminalRoute {
    Register,
    Poll,
    OutputPoll,
    ControlUpload,
    OutputUpload,
}
#[derive(Clone, Copy)]
pub enum RunnerRoute {
    Material,
    Prepared,
    Started,
    Renew,
    Observations,
    Terminal(TerminalRoute),
    Artifact,
    Results,
    Stopped,
    MailPoll,
    MailAck,
}
#[derive(Clone, Copy)]
pub enum SourceMember {
    Manifest,
    Bundle,
}
impl RunnerRoute {
    fn path(self) -> &'static str {
        match self {
            Self::Material => "/v1/worker/profiled/material",
            Self::Prepared => "/v1/worker/profiled-v2/prepare",
            Self::Started => "/v1/worker/profiled-v2/started",
            Self::Renew => "/v1/worker/profiled/renew",
            Self::Observations => "/v1/worker/observations/batch",
            Self::Artifact => "/v1/worker/profiled/artifact",
            Self::Results => "/v1/worker/profiled/results",
            Self::Stopped => "/v1/worker/profiled/stopped",
            Self::MailPoll => "/v2/worker/messages/poll",
            Self::MailAck => "/v2/worker/messages/ack",
            Self::Terminal(route) => match route {
                TerminalRoute::Register => "/v1/terminal-relay/register",
                TerminalRoute::Poll => "/v1/terminal-relay/poll",
                TerminalRoute::OutputPoll => "/v1/terminal-relay/output-poll",
                TerminalRoute::ControlUpload => "/v1/terminal-relay/control-upload",
                TerminalRoute::OutputUpload => "/v1/terminal-relay/upload",
            },
        }
    }
    fn operation(self, scope: PermitScope) -> Result<RunnerOperation> {
        use RunnerOperation::*;
        if scope == PermitScope::Preparation {
            return match self {
                Self::Material | Self::Artifact => Ok(FetchPreparationMaterial),
                Self::Prepared => Ok(Prepared),
                _ => anyhow::bail!("execution requires a committed grant"),
            };
        }
        Ok(match self {
            Self::Material | Self::Artifact => FetchExecutionMaterial,
            Self::Prepared => anyhow::bail!("Prepared requires preparation scope"),
            Self::Started => Started,
            Self::Renew => Renew,
            Self::Observations => Observe,
            Self::Terminal(_) => Terminal,
            Self::Results | Self::Stopped => ReportResult,
            Self::MailPoll => MailboxPoll,
            Self::MailAck => MailboxAck,
        })
    }
}
struct Authority {
    permit: DelegatedPermit,
    context: PermitContext,
    handoff: RunnerHandoff,
    deadline_ns: u64,
    hard_ns: u64,
    revision: Option<u64>,
}
struct Shared {
    authority: Mutex<Authority>,
    lost: AtomicBool,
}
#[derive(Clone)]
pub struct RunnerHttp {
    http: reqwest::Client,
    origin: Url,
    shared: Arc<Shared>,
}
/// Refusals are typed without retaining the response body or credentials.
#[derive(Debug)]
pub struct RunnerRefused(pub StatusCode);
impl std::fmt::Display for RunnerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runner authority refused ({})", self.0)
    }
}
impl std::error::Error for RunnerRefused {}

fn at_boot(server_deadline: u64, handoff: &RunnerHandoff) -> Result<u64> {
    server_deadline
        .checked_sub(handoff.delivery.clock.server_time_ms)
        .and_then(|v| v.checked_mul(1_000_000))
        .and_then(|v| v.checked_add(handoff.sent_boot_ns))
        .ok_or_else(|| anyhow::anyhow!("runner deadline overflow"))
}
fn server_now(handoff: &RunnerHandoff, now: u64) -> Result<u64> {
    // sent, rather than received, conservatively includes all network delay.
    handoff
        .delivery
        .clock
        .server_time_ms
        .checked_add(
            now.checked_sub(handoff.sent_boot_ns)
                .ok_or_else(|| anyhow::anyhow!("runner clock moved backwards"))?
                / 1_000_000,
        )
        .ok_or_else(|| anyhow::anyhow!("runner clock overflow"))
}
/// Holds stay below the 22 s request timeout; no proxy, redirect or retry.
fn pool() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(22))
        .build()?)
}
fn context(
    permit: &DelegatedPermit,
    handoff: &RunnerHandoff,
    now_ms: u64,
) -> Result<PermitContext> {
    ensure!(
        permit.binding == handoff.binding,
        "runner handoff binding changed"
    );
    let (phase, grant_expires_at_ms, hard_deadline_ms) = match &handoff.authority {
        RunnerAuthority::Preparation(authority) => {
            authority.validate_permit(permit, RunnerOperation::Prepared, now_ms)?;
            (
                GrantPhase::Preparing,
                authority.preparation_deadline_ms,
                authority.preparation_deadline_ms,
            )
        }
        RunnerAuthority::Execution(authority) => {
            authority.validate_handoff(permit, now_ms)?;
            (
                GrantPhase::Granted,
                authority.grant.expires_at_ms,
                authority.hard_deadline_ms,
            )
        }
    };
    Ok(PermitContext {
        binding: handoff.binding.clone(),
        phase,
        grant_expires_at_ms,
        hard_deadline_ms,
    })
}
impl RunnerHttp {
    pub fn from_file(origin: &str, path: &Path, handoff: RunnerHandoff) -> Result<Self> {
        let bytes = RunnerFiles::read_secret(path)?;
        let permit: DelegatedPermit = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid runner permit file"))?;
        let origin = Url::parse(origin).map_err(|_| anyhow::anyhow!("invalid runner origin"))?;
        let local = origin.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            origin.host_str().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none()
                && (origin.scheme() == "https" || (origin.scheme() == "http" && local)),
            "invalid runner origin"
        );
        let now = super::boot_ns()?;
        ensure!(
            handoff.sent_boot_ns <= handoff.received_boot_ns && handoff.received_boot_ns <= now,
            "runner delivery observation order"
        );
        ensure!(
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
                == handoff.kernel_boot_id,
            "runner handoff kernel boot mismatch"
        );
        handoff
            .delivery
            .validate_for(handoff.delivery.nonce, permit.expires_at_ms)?;
        let context = context(&permit, &handoff, server_now(&handoff, now)?)?;
        let hard_ns = at_boot(permit.hard_deadline_ms, &handoff)?;
        let deadline_ns = handoff
            .delivery
            .client_deadline_ns(
                handoff.sent_boot_ns,
                handoff.received_boot_ns,
                Some(hard_ns),
            )?
            .min(at_boot(permit.expires_at_ms, &handoff)?);
        ensure!(now < deadline_ns, "runner permit expired");
        Ok(Self {
            http: pool()?,
            origin,
            shared: Arc::new(Shared {
                authority: Mutex::new(Authority {
                    permit,
                    context,
                    handoff,
                    deadline_ns,
                    hard_ns,
                    revision: None,
                }),
                lost: AtomicBool::new(false),
            }),
        })
    }
    pub fn for_execution(origin: &str, path: &Path, frozen: &FrozenLaunch) -> Result<Self> {
        frozen.validate_local(&frozen.state_dir)?;
        let binding = serde_json::from_slice::<DelegatedPermit>(&RunnerFiles::read_secret(path)?)
            .map_err(|_| anyhow::anyhow!("invalid runner permit file"))?
            .binding;
        Self::from_file(
            origin,
            path,
            RunnerHandoff {
                authority: RunnerAuthority::Execution(frozen.authority.clone()),
                binding,
                delivery: frozen.delivery.clone(),
                sent_boot_ns: frozen.sent_boot_ns,
                received_boot_ns: frozen.received_boot_ns,
                kernel_boot_id: frozen.kernel_boot_id.clone(),
            },
        )
    }
    /// Bind the transport to the exact launch handoff before preflight or spawn.
    pub(crate) fn bind_execution(
        &self,
        frozen: &FrozenLaunch,
        permit: &DelegatedPermit,
    ) -> Result<()> {
        let mut state = self.shared.authority.lock().unwrap();
        self.live(&state)?;
        let RunnerAuthority::Execution(authority) = &state.handoff.authority else {
            anyhow::bail!("runner not granted");
        };
        ensure!(
            state.permit == *permit
                && serde_json::to_value(authority)? == serde_json::to_value(&frozen.authority)?
                && serde_json::to_value(&state.handoff.delivery)?
                    == serde_json::to_value(&frozen.delivery)?
                && state.handoff.sent_boot_ns == frozen.sent_boot_ns
                && state.handoff.received_boot_ns == frozen.received_boot_ns
                && state.handoff.kernel_boot_id == frozen.kernel_boot_id
                && state.revision.is_none_or(|v| v == frozen.revision),
            "runner transport launch binding mismatch"
        );
        state.revision = Some(frozen.revision);
        Ok(())
    }
    /// Same permit authority and loss state on a separate connection pool, so
    /// per-sandbox terminal/observation channels never queue behind (or take
    /// connections from) renewal and cancellation traffic.
    pub fn channel_pool(&self) -> Result<Self> {
        Ok(Self {
            http: pool()?,
            origin: self.origin.clone(),
            shared: self.shared.clone(),
        })
    }
    /// Launch binding of the current authority (preparation or execution).
    pub fn launch_binding(&self) -> LaunchBinding {
        match &self.shared.authority.lock().unwrap().handoff.authority {
            RunnerAuthority::Preparation(p) => p.binding.clone(),
            RunnerAuthority::Execution(e) => e.grant.binding.clone(),
        }
    }
    pub fn binding(&self) -> PermitBinding {
        self.shared.authority.lock().unwrap().permit.binding.clone()
    }
    pub fn context(&self) -> PermitContext {
        self.shared.authority.lock().unwrap().context.clone()
    }
    pub fn authority_lost(&self) -> bool {
        if self.shared.lost.load(Ordering::SeqCst) {
            return true;
        }
        let state = self.shared.authority.lock().unwrap();
        if self.live(&state).is_err() {
            self.lose_authority();
            return true;
        }
        false
    }
    pub(crate) fn lose_authority(&self) {
        self.shared.lost.store(true, Ordering::SeqCst);
    }
    fn live(&self, state: &Authority) -> Result<u64> {
        ensure!(
            !self.shared.lost.load(Ordering::SeqCst),
            "runner authority lost"
        );
        let now = super::boot_ns()?;
        ensure!(
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
                == state.handoff.kernel_boot_id,
            "runner kernel boot mismatch"
        );
        ensure!(
            now >= state.handoff.received_boot_ns && now < state.deadline_ns && now < state.hard_ns,
            "runner permit expired"
        );
        server_now(&state.handoff, now)
    }
    fn authorize(
        &self,
        route: RunnerRoute,
        binding: &PermitBinding,
        body: &Value,
    ) -> Result<header::HeaderValue> {
        let state = self.shared.authority.lock().unwrap();
        let now = self.live(&state).inspect_err(|_| self.lose_authority())?;
        ensure!(
            *binding == state.permit.binding,
            "runner request binding mismatch"
        );
        let session = match &state.handoff.authority {
            RunnerAuthority::Execution(authority) => match &authority.grant.binding.session {
                SessionIdentity::Remote { session_id, .. } => Some(*session_id),
                _ => None,
            },
            RunnerAuthority::Preparation(_) => None,
        };
        check_body(body, binding, session)?;
        state
            .permit
            .validate_for(&state.context, route.operation(state.permit.scope)?, now)?;
        let mut value =
            header::HeaderValue::from_str(&format!("Bearer {}", state.permit.credential.expose()))?;
        value.set_sensitive(true);
        Ok(value)
    }
    pub(crate) fn check_scope(&self, route: RunnerRoute, binding: &PermitBinding) -> Result<()> {
        self.authorize(route, binding, &Value::Null).map(|_| ())
    }
    pub(crate) fn check_renewal_request(&self, request: &RunnerRenewalRequest) -> Result<()> {
        request.validate()?;
        self.check_scope(RunnerRoute::Renew, &request.permit.binding)?;
        let state = self.shared.authority.lock().unwrap();
        let RunnerAuthority::Execution(authority) = &state.handoff.authority else {
            anyhow::bail!("runner not granted");
        };
        ensure!(
            request.permit == state.permit
                && state.revision.is_none_or(|v| v == request.revision)
                && serde_json::to_value(authority)? == serde_json::to_value(&request.authority)?,
            "runner renewal request changed"
        );
        Ok(())
    }
    pub async fn post(
        &self,
        route: RunnerRoute,
        binding: &PermitBinding,
        request: &impl Serialize,
    ) -> Result<Vec<u8>> {
        self.post_bounded(route, binding, request, MAX_BODY, MAX_BODY)
            .await
    }
    pub async fn post_bounded(
        &self,
        route: RunnerRoute,
        binding: &PermitBinding,
        request: &impl Serialize,
        max_request: usize,
        max_response: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            max_request <= MAX_BODY && max_response <= MAX_BODY,
            "runner body bound"
        );
        let body = serde_json::to_value(request)?;
        let bytes = serde_json::to_vec(&body)?;
        ensure!(bytes.len() <= max_request, "runner request size");
        let authorization = self.authorize(route, binding, &body)?;
        let response = self
            .http
            .post(self.origin.join(route.path())?)
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("runner transport unavailable; disposition unknown"))?;
        self.status(&response)?;
        self.read(response, max_response).await
    }
    pub async fn source(
        &self,
        binding: &PermitBinding,
        member: SourceMember,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<u8>> {
        ensure!((1..=262_144).contains(&limit), "runner source range");
        let member = match member {
            SourceMember::Manifest => "manifest",
            SourceMember::Bundle => "bundle",
        };
        let response = self
            .source_get(
                binding,
                &format!(
                    "{}/{member}?offset={offset}&limit={limit}",
                    binding.attempt_id
                ),
            )
            .await?;
        self.read(response, MAX_BODY).await
    }
    async fn source_get(&self, binding: &PermitBinding, path: &str) -> Result<reqwest::Response> {
        let authorization = self.authorize(RunnerRoute::Material, binding, &Value::Null)?;
        let response = self
            .http
            .get(
                self.origin
                    .join(&format!("/v1/worker/profiled/source/{path}"))?,
            )
            .header(header::AUTHORIZATION, authorization)
            .header("aq-source-api-version", "1")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("runner source unavailable"))?;
        self.status(&response)?;
        Ok(response)
    }
    /// Retained source descriptor for this permit's attempt, bound to the
    /// current launch (attempt, session, generation and resolved spec).
    pub async fn source_status(&self, binding: &PermitBinding) -> Result<TransferManifest> {
        let launch = self.launch_binding();
        let response = self
            .source_get(binding, &binding.attempt_id.to_string())
            .await?;
        crate::native::http::source::version(&response)?;
        ensure!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v == "application/json"),
            "runner source status body type"
        );
        let manifest: TransferManifest = serde_json::from_slice(&self.read(response, 8192).await?)
            .map_err(|_| anyhow::anyhow!("invalid runner source status"))?;
        crate::native::http::source::transfer(&launch, &manifest)?;
        Ok(manifest)
    }
    /// One exact range of a retained member; the response must restate the
    /// same descriptor and offset and carry exactly the requested byte count.
    pub async fn source_chunk(
        &self,
        binding: &PermitBinding,
        expected: &TransferManifest,
        member: TransferFile,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<u8>> {
        crate::native::http::source::transfer(&self.launch_binding(), expected)?;
        let (name, size) = match member {
            TransferFile::Manifest => ("manifest", expected.manifest_bytes),
            TransferFile::Bundle => ("bundle", expected.bundle_bytes),
        };
        ensure!(
            (1..=262_144).contains(&limit) && offset < size,
            "runner source range"
        );
        let count = (size - offset).min(u64::from(limit)) as usize;
        let response = self
            .source_get(
                binding,
                &format!(
                    "{}/{name}?offset={offset}&limit={limit}",
                    binding.attempt_id
                ),
            )
            .await?;
        crate::native::http::source::version(&response)?;
        crate::native::http::body_type(&response)?;
        let manifest: TransferManifest =
            crate::native::http::metadata(&response, "aq-source-transfer")?;
        ensure!(&manifest == expected, "runner source transfer changed");
        let offsets: Vec<_> = response
            .headers()
            .get_all("aq-source-offset")
            .iter()
            .collect();
        ensure!(
            offsets.len() == 1
                && offsets[0].to_str().ok().and_then(|v| v.parse::<u64>().ok()) == Some(offset),
            "runner source offset changed"
        );
        let bytes = self.read(response, count).await?;
        ensure!(bytes.len() == count, "runner source short chunk");
        Ok(bytes)
    }
    fn status(&self, response: &reqwest::Response) -> Result<()> {
        let status = response.status();
        // A server error/rate limit is availability loss; explicit route refusals
        // (including redirects) revoke all channels immediately. Never forward.
        if !status.is_success() {
            if !status.is_server_error()
                && status != StatusCode::TOO_MANY_REQUESTS
                && status != StatusCode::REQUEST_TIMEOUT
            {
                self.lose_authority();
                return Err(RunnerRefused(status).into());
            }
            anyhow::bail!("runner server unavailable");
        }
        Ok(())
    }
    async fn read(&self, mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
        ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "runner response size"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("runner response unknown"))?
        {
            ensure!(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                "runner response size"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(
            !self.authority_lost(),
            "runner authority lost during request"
        );
        Ok(bytes)
    }
    /// Called only on an authenticated, correlated Prepared reply. The B2 codec
    /// remains injected until its published envelopes are available.
    pub fn install_grant(
        &self,
        request: &PermitUpgradeRequest,
        response: &PermitUpgradeResponse,
        delivery: DeliveryStamp,
        sent: u64,
        received: u64,
    ) -> Result<()> {
        let mut state = self.shared.authority.lock().unwrap();
        let now = self.live(&state)?;
        let RunnerAuthority::Preparation(preparation) = &state.handoff.authority else {
            anyhow::bail!("runner already granted");
        };
        request.validate_for(preparation, &state.permit, now)?;
        response.validate_for(request, preparation, &state.permit, now)?;
        let handoff = RunnerHandoff {
            authority: RunnerAuthority::Execution(response.authority.clone()),
            binding: state.handoff.binding.clone(),
            delivery,
            sent_boot_ns: sent,
            received_boot_ns: received,
            kernel_boot_id: state.handoff.kernel_boot_id.clone(),
        };
        ensure!(
            sent >= state.handoff.received_boot_ns
                && sent <= received
                && received <= super::boot_ns()?,
            "runner grant observation order"
        );
        handoff
            .delivery
            .validate_for(handoff.delivery.nonce, response.permit.expires_at_ms)?;
        let context = context(&response.permit, &handoff, server_now(&handoff, received)?)?;
        let hard_ns = at_boot(response.permit.hard_deadline_ms, &handoff)?;
        let deadline_ns = handoff
            .delivery
            .client_deadline_ns(sent, received, Some(hard_ns))?
            .min(at_boot(response.permit.expires_at_ms, &handoff)?);
        ensure!(
            super::boot_ns()? < deadline_ns,
            "runner grant arrived expired"
        );
        *state = Authority {
            permit: response.permit.clone(),
            context,
            handoff,
            deadline_ns,
            hard_ns,
            revision: None,
        };
        Ok(())
    }
    pub(crate) fn accept_renewal(
        &self,
        request: &RunnerRenewalRequest,
        response: &super::RenewalDelivery,
        nonce: Uuid,
        sent: u64,
        received: u64,
    ) -> Result<()> {
        let mut state = self.shared.authority.lock().unwrap();
        self.live(&state)?;
        ensure!(
            state.revision.is_none_or(|v| v == request.revision),
            "runner renewal revision mismatch"
        );
        ensure!(
            request.permit == state.permit
                && response.context.binding == state.context.binding
                && serde_json::to_value(&response.context)?
                    == serde_json::to_value(&state.context)?,
            "runner renewal prior context mismatch"
        );
        ensure!(
            sent >= state.handoff.received_boot_ns
                && sent <= received
                && received <= super::boot_ns()?
                && response.server_now_ms >= state.handoff.delivery.clock.server_time_ms,
            "runner renewal clock mismatch"
        );
        response
            .response
            .validate_for(request, &state.context, nonce, response.server_now_ms)?;
        let deadline = response.response.client_deadline_ns(
            sent,
            received,
            state.deadline_ns,
            state.hard_ns,
            request.revision,
        )?;
        if let RenewalReply::Renewed { permit, .. } = &response.response.reply {
            state.permit = permit.clone();
            state.context.grant_expires_at_ms = permit.expires_at_ms;
            state.deadline_ns = deadline;
            state.revision = Some(response.response.revision);
        }
        // AwaitingNodeReadoption deliberately changes no deadline or identity.
        Ok(())
    }
}
fn check_body(value: &Value, binding: &PermitBinding, session: Option<Uuid>) -> Result<()> {
    match value {
        Value::Array(values) => {
            for v in values {
                check_body(v, binding, session)?;
            }
        }
        Value::Object(values) => {
            for (key, v) in values {
                let expected = match key.as_str() {
                    "attempt_id" => Some(binding.attempt_id),
                    "node_id" => Some(binding.node.node_id),
                    "boot_id" => Some(binding.node.boot_id),
                    "sandbox_id" => Some(binding.sandbox_id),
                    "channel_id" => Some(binding.channel_id),
                    "session_id" => Some(session.ok_or_else(|| {
                        anyhow::anyhow!("runner session requires execution grant")
                    })?),
                    _ => None,
                };
                if let Some(id) = expected {
                    ensure!(
                        v.as_str().and_then(|s| Uuid::parse_str(s).ok()) == Some(id),
                        "runner body identity mismatch"
                    );
                }
                if matches!(key.as_str(), "assignment_generation" | "grant_generation") {
                    ensure!(
                        v.as_u64() == Some(binding.grant_generation),
                        "runner body generation mismatch"
                    );
                }
                check_body(v, binding, session)?;
            }
        }
        _ => {}
    }
    Ok(())
}
