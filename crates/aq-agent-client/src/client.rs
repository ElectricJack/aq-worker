//! Direct outbound agent API access. No node credential or arbitrary route API.
use crate::{
    context::{ContextBundle, MAX_BUNDLE_BYTES},
    files::PrivateDir,
    journal::{Journal, Stamp, terminal},
};
use anyhow::{Context as _, Result, ensure};
use aq_agent_api::{Binding, Context, canonical, parse, valid};
use reqwest::{Method, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub format_version: u32,
    pub server_url: String,
    pub binding: Binding,
    /// An explicitly provisioned agent egress credential, never copied from the
    /// worker's node credential/configuration by the client.
    pub edge_authentication: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionCredential {
    pub api_version: u32,
    pub token: String,
    pub context: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEgressCredential {
    pub client_id: String,
    pub client_secret: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub code: &'static str,
    pub operation_id: Option<Uuid>,
    pub effect: &'static str,
    pub retryable: bool,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for Failure {}
impl Failure {
    fn uncertain(id: Option<Uuid>) -> Self {
        Self {
            code: "remote_response_unknown",
            operation_id: id,
            effect: if id.is_some() { "unknown" } else { "read_only" },
            retryable: false,
        }
    }
}
pub struct Outcome {
    pub value: Value,
    pub exit_code: u8,
}
pub(crate) struct Http {
    client: reqwest::Client,
    origin: reqwest::Url,
}
impl Http {
    pub fn new(origin: &str) -> Result<Self> {
        let origin =
            reqwest::Url::parse(origin).map_err(|_| anyhow::anyhow!("invalid AQ origin"))?;
        ensure!(
            matches!(origin.scheme(), "https" | "http")
                && origin.host_str().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.path() == "/",
            "AQ endpoint must be an HTTP origin without credentials"
        );
        ensure!(
            origin.scheme() == "https"
                || origin.host_str().is_some_and(|h| h == "localhost"
                    || h == "[::1]"
                    || h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())),
            "unencrypted AQ access is limited to loopback fixtures"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { client, origin })
    }
    pub async fn exchange(
        &self,
        method: Method,
        path: &str,
        token: &str,
        edge: Option<&AgentEgressCredential>,
        body: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(u16, Value), Failure> {
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Failure::uncertain(None))?;
        bearer.set_sensitive(true);
        let url = self
            .origin
            .join(path)
            .map_err(|_| Failure::uncertain(None))?;
        let mut request = self
            .client
            .request(method, url)
            .header(header::AUTHORIZATION, bearer)
            .header("X-AQ-Agent-API-Version", "1");
        if let Some(edge) = edge {
            for (key, value) in [
                ("cf-access-client-id", &edge.client_id),
                ("cf-access-client-secret", &edge.client_secret),
            ] {
                let mut value =
                    header::HeaderValue::from_str(value).map_err(|_| Failure::uncertain(None))?;
                value.set_sensitive(true);
                request = request.header(key, value);
            }
        }
        if let Some(body) = body {
            if body.len() > aq_agent_api::MAX_DOCUMENT_BYTES {
                return Err(Failure::uncertain(None));
            }
            // Mutations carry the CSRF marker the agent routes document.
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .header("X-AQ-Request", "1")
                .body(body);
        }
        let mut response = request.send().await.map_err(|_| Failure::uncertain(None))?;
        let code = response.status().as_u16();
        if response
            .headers()
            .get("X-AQ-Agent-API-Version")
            .is_none_or(|h| h != "1")
            || response.content_length().is_some_and(|n| n > limit as u64)
        {
            return Err(Failure::uncertain(None));
        }
        let mut bytes = vec![];
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::uncertain(None))?
        {
            if chunk.len() > limit - bytes.len() {
                return Err(Failure::uncertain(None));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = parse(&bytes).map_err(|_| Failure::uncertain(None))?;
        Ok((code, body))
    }
}

pub struct AgentClient {
    config: ClientConfig,
    directory: PrivateDir,
    bundle: ContextBundle,
    journal: Journal,
    http: Http,
}
impl AgentClient {
    pub fn open(root: &Path) -> Result<Self> {
        let directory = PrivateDir::open(root)?;
        let config: ClientConfig =
            serde_json::from_value(parse(&directory.read("client.json", 16_384)?)?)?;
        ensure!(config.format_version == 1, "client configuration version");
        config.binding.validate()?;
        let raw = directory.read("context.json", MAX_BUNDLE_BYTES)?;
        let aq_agent_api::StrictValue(value) = serde_json::from_slice(&raw)?;
        let bundle: ContextBundle = serde_json::from_value(value)?;
        bundle.validate()?;
        ensure!(
            bundle.binding == config.binding,
            "client context binding mismatch"
        );
        let http = Http::new(&config.server_url)?;
        let journal = Journal::open(
            root,
            config.binding.clone(),
            &bundle.graph_context["profile"]["retry_budget"],
        )?;
        Ok(Self {
            config,
            directory,
            bundle,
            journal,
            http,
        })
    }
    pub fn cached_context(&self) -> Result<Value> {
        self.bundle.cached_view()
    }
    pub fn binding(&self) -> &Binding {
        &self.config.binding
    }
    pub async fn query(&self, query: Value) -> Result<Outcome> {
        self.read(json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":self.config.binding,"query":query})).await
    }
    fn credential(&self, now: &Stamp) -> Result<SessionCredential> {
        let secret: SessionCredential = serde_json::from_value(parse(
            &self
                .directory
                .read("credential.json", aq_agent_api::MAX_DOCUMENT_BYTES)?,
        )?)?;
        ensure!(
            secret.api_version == 1
                && secret.token.starts_with("aqa_")
                && (36..=4096).contains(&secret.token.len())
                && !secret.token.chars().any(char::is_whitespace),
            "agent credential class or shape refused"
        );
        Context::new(secret.context.clone(), &self.config.binding)?;
        let expires = chrono::DateTime::parse_from_rfc3339(
            secret.context["credential"]["expires_at"]
                .as_str()
                .context("credential expiry absent")?,
        )?
        .timestamp_millis();
        ensure!(
            expires > 0 && now.unix_ms < expires as u64,
            "agent credential expired; node rotation/recovery required"
        );
        Ok(secret)
    }
    fn edge(&self) -> Result<Option<AgentEgressCredential>> {
        if self.config.edge_authentication {
            let value = parse(&self.directory.read("agent-egress.json", 16_384)?)?;
            Ok(Some(serde_json::from_value(value)?))
        } else {
            Ok(None)
        }
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        now: &Stamp,
    ) -> Result<(u16, Value)> {
        let secret = self.credential(now)?;
        let edge = self.edge()?;
        let limit = secret.context["profile"]["max_response_bytes"]
            .as_u64()
            .context("response cap absent")? as usize;
        self.http
            .exchange(method, path, &secret.token, edge.as_ref(), body, limit)
            .await
            .map_err(Into::into)
    }
    pub async fn context(&self) -> Result<Outcome> {
        let (status, value) = self
            .request(Method::GET, "/v1/agent/context", None, &Stamp::now()?)
            .await?;
        if status != 200 {
            return remote_refusal(status, value, None);
        }
        Context::new(value.clone(), &self.config.binding)?;
        Ok(Outcome {
            value,
            exit_code: 0,
        })
    }
    pub async fn read(&self, request: Value) -> Result<Outcome> {
        ensure!(
            valid("ReadRequest", &request)
                && request["binding"] == serde_json::to_value(&self.config.binding)?,
            "read request contract/binding refused"
        );
        let (status, value) = self
            .request(
                Method::POST,
                "/v1/agent/graph/read",
                Some(canonical(&request)?),
                &Stamp::now()?,
            )
            .await?;
        if status != 200 {
            return remote_refusal(status, value, None);
        }
        ensure!(
            valid("ReadResponse", &value)
                && value["binding"] == request["binding"]
                && value["request_id"] == request["request_id"]
                && value["result"]["kind"] == request["query"]["kind"],
            "read response association refused"
        );
        Ok(Outcome {
            value,
            exit_code: 0,
        })
    }
    /// One send only. Repeated calls retain the original ID, bytes and cumulative
    /// retry budget. Unknown responses are never described as applied/not-applied.
    pub async fn command(&mut self, request: Value) -> Result<Outcome> {
        ensure!(
            canonical(&request)?.len() <= aq_agent_api::MAX_DOCUMENT_BYTES,
            "command document bound exceeded"
        );
        let id = self.journal.prepare(
            request,
            &self.bundle.graph_context["profile"]["retry_budget"],
        )?;
        let old = self.journal.operation(id)?;
        if let Some(value) = &old.response
            && terminal(value)
        {
            return Ok(operation_outcome(value.clone()));
        }
        let now = Stamp::now()?;
        // Preflight failures before send keep previously uncertain intent intact.
        let _credential = self.credential(&now).map_err(|_| Failure {
            code: "authentication_or_authority_refused",
            operation_id: Some(id),
            effect: "unknown",
            retryable: false,
        })?;
        let policy = &self.bundle.graph_context["profile"]["retry_budget"];
        let ceiling = policy["initial_delay_ms"]
            .as_u64()
            .unwrap_or(250)
            .saturating_mul(1u64 << old.attempts.min(16))
            .min(policy["max_delay_ms"].as_u64().unwrap_or(2000));
        // Equal jitter is persisted with the send boundary. Reopening the
        // process cannot redraw a shorter deadline for an uncertain send.
        let entropy = Uuid::new_v4().as_u128() as u64;
        let delay = ceiling / 2 + entropy % (ceiling - ceiling / 2 + 1);
        let bytes = self
            .journal
            .begin_send(id, now.clone(), delay)
            .map_err(|_| Failure {
                code: "local_retry_budget_or_persistence_refused",
                operation_id: Some(id),
                effect: "unknown",
                retryable: false,
            })?;
        match self
            .request(Method::POST, "/v1/agent/graph/commands", Some(bytes), &now)
            .await
        {
            Ok((status, value)) if valid("OperationStatus", &value) => {
                if !operation_http_status(status, &value) {
                    return Err(Failure::uncertain(Some(id)).into());
                }
                self.journal
                    .record_response(id, value.clone())
                    .map_err(|_| Failure::uncertain(Some(id)))?;
                Ok(operation_outcome(value))
            }
            Ok((status, value)) => remote_refusal(status, value, Some(id)),
            Err(_) => Err(Failure::uncertain(Some(id)).into()),
        }
    }
    pub async fn operation(&mut self, id: Uuid) -> Result<Outcome> {
        ensure!(!id.is_nil(), "operation ID required");
        let known = self
            .journal
            .operation(id)
            .ok()
            .map(|op| op.payload_digest.clone());
        let path = format!("/v1/agent/operations/{id}");
        match self.request(Method::GET, &path, None, &Stamp::now()?).await {
            Ok((status, value)) if valid("OperationStatus", &value) => {
                if !operation_http_status(status, &value)
                    || value["operation_id"] != id.to_string()
                    || value["binding"] != serde_json::to_value(&self.config.binding)?
                {
                    return Err(Failure::uncertain(Some(id)).into());
                }
                crate::journal::check_operation_response(
                    &value,
                    id,
                    known
                        .as_deref()
                        .unwrap_or_else(|| value["payload_digest"].as_str().unwrap()),
                    &self.config.binding,
                )
                .map_err(|_| Failure::uncertain(Some(id)))?;
                if known.is_some() {
                    self.journal
                        .record_response(id, value.clone())
                        .map_err(|_| Failure::uncertain(Some(id)))?;
                }
                Ok(operation_outcome(value))
            }
            Ok((status, value)) => remote_refusal(status, value, Some(id)),
            Err(_) => Err(Failure::uncertain(Some(id)).into()),
        }
    }
}
/// Agent mailbox routes. Harness delivery and node polling are not part of
/// this client; replies are sends built from the original message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRoute {
    Send,
    Inbox,
    Ack,
    Body,
}
impl MessageRoute {
    fn contract(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Send => (
                "/v1/agent/messages/send",
                "MessageSend",
                "MessageSendReceipt",
            ),
            Self::Inbox => (
                "/v1/agent/messages/inbox",
                "MessageInbox",
                "MessageInboxResponse",
            ),
            Self::Ack => ("/v1/agent/messages/ack", "MessageAck", "MessageAckReceipt"),
            Self::Body => (
                "/v1/agent/messages/body",
                "MessageBodyRequest",
                "MessageBodyResponse",
            ),
        }
    }
}
impl AgentClient {
    /// The bound task mailbox, or the bound session mailbox.
    pub fn mailbox(&self, session: bool) -> Value {
        let b = &self.config.binding;
        let mut value = json!({"kind":"task","workspace_id":b.workspace_id,"project_id":b.project_id,"task_id":b.task_id});
        if session {
            value["kind"] = json!("session");
            value["session_id"] = json!(b.session_id);
            value["attempt_id"] = json!(b.attempt_id);
            value["generation"] = json!(b.generation);
        }
        value
    }
    pub fn inbox_request(
        &self,
        session: bool,
        after: &str,
        limit: u8,
        thread: Option<Uuid>,
    ) -> Value {
        json!({"recipient":self.mailbox(session),"after":after,"thread_id":thread,"limit":limit})
    }
    /// One send per call. Send/ack carry caller-chosen operation IDs; an
    /// unknown reply reports that ID and never implies delivery.
    pub async fn message(&self, route: MessageRoute, request: Value) -> Result<Outcome> {
        let (path, request_schema, response_schema) = route.contract();
        ensure!(
            crate::mail::valid(request_schema, &request),
            "message request contract refused"
        );
        let recipient = &request["recipient"];
        let scoped = if route == MessageRoute::Send {
            // Sends stay inside the credential's workspace/project scope, or go
            // to the supervisor conversation; the daemon decides whether the
            // named mailbox accepts this sender.
            crate::mail::supervisor_mailbox(recipient)
                || recipient["workspace_id"] == self.mailbox(false)["workspace_id"]
                    && recipient["project_id"] == self.mailbox(false)["project_id"]
        } else {
            *recipient == self.mailbox(false) || *recipient == self.mailbox(true)
        };
        ensure!(scoped, "message recipient outside the bound mailbox scope");
        let operation_id = request["operation_id"]
            .as_str()
            .and_then(|id| id.parse::<Uuid>().ok());
        let now = Stamp::now()?;
        // Nothing is sent with an expired or malformed credential.
        self.credential(&now).map_err(|_| Failure {
            code: "authentication_or_authority_refused",
            operation_id,
            effect: "not_asserted",
            retryable: false,
        })?;
        let (status, value) = match self
            .request(Method::POST, path, Some(canonical(&request)?), &now)
            .await
        {
            Ok(reply) => reply,
            Err(_) => return Err(Failure::uncertain(operation_id).into()),
        };
        if status != 200 {
            return remote_refusal(status, value, operation_id);
        }
        let associated = crate::mail::valid(response_schema, &value)
            && match route {
                MessageRoute::Send | MessageRoute::Ack => {
                    value["operation_id"] == request["operation_id"]
                        && value["message_id"] == request["message_id"]
                        && value["recipient"] == request["recipient"]
                }
                MessageRoute::Inbox => {
                    value["recipient"] == request["recipient"] && value["after"] == request["after"]
                }
                MessageRoute::Body => {
                    value["message_id"] == request["message_id"]
                        && value["offset"] == request["offset"]
                }
            };
        if !associated {
            return Err(Failure::uncertain(operation_id).into());
        }
        Ok(Outcome {
            value,
            exit_code: 0,
        })
    }
}
/// Inbox pages scanned by one reply lookup (32 messages each).
const MAX_REPLY_PAGES: usize = 64;
/// Body chunks followed by one body retrieval.
const MAX_BODY_CHUNKS: usize = 64;
fn cursor(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}
impl AgentClient {
    /// The original message in the bound mailbox, with its sender recovered
    /// from a one-message page when the listing omitted its body.
    async fn original(&self, session: bool, message_id: Uuid) -> Result<Value> {
        let id = json!(message_id);
        let mut after = 0u64;
        for _ in 0..MAX_REPLY_PAGES {
            let request = self.inbox_request(session, &after.to_string(), 32, None);
            let page = self.message(MessageRoute::Inbox, request).await?.value;
            let messages = page["messages"].as_array().cloned().unwrap_or_default();
            if let Some(entry) = messages.iter().find(|m| m["message_id"] == id) {
                if entry["body_omitted"] != true {
                    return Ok(entry.clone());
                }
                let before = cursor(&entry["cursor"])
                    .and_then(|c| c.checked_sub(1))
                    .context("message cursor")?;
                let request = self.inbox_request(session, &before.to_string(), 1, None);
                let page = self.message(MessageRoute::Inbox, request).await?.value;
                return page["messages"]
                    .as_array()
                    .and_then(|m| m.first())
                    .filter(|m| m["message_id"] == id && m["body_omitted"] != true)
                    .cloned()
                    .context("original message sender unavailable");
            }
            let next = messages
                .iter()
                .filter_map(|m| cursor(&m["cursor"]))
                .chain(cursor(&page["next_cursor"]))
                .max()
                .unwrap_or(after);
            if page["has_more"] != true || next <= after {
                break;
            }
            after = next;
        }
        anyhow::bail!("original message not found in the bound mailbox")
    }
    /// Reply on the original thread to the original sender's mailbox. The
    /// caller-chosen operation and message IDs make a retry idempotent.
    pub async fn reply(
        &self,
        session: bool,
        message_id: Uuid,
        body: String,
        operation_id: Uuid,
        reply_id: Uuid,
    ) -> Result<Outcome> {
        crate::mail::check_body(&body)?;
        let original = self.original(session, message_id).await?;
        let recipient = crate::mail::reply_recipient(&original["sender"])?;
        let send = json!({"operation_id":operation_id,"message_id":reply_id,
            "thread_id":original["thread_id"],"recipient":recipient,"body":body});
        self.message(MessageRoute::Send, send).await
    }
    /// Every chunk of one message body from the bound mailbox, one digest.
    pub async fn body(&self, session: bool, message_id: Uuid) -> Result<Outcome> {
        let (mut offset, mut body, mut first) = ("0".to_owned(), String::new(), None::<Value>);
        for _ in 0..MAX_BODY_CHUNKS {
            let request = json!({"recipient":self.mailbox(session),"message_id":message_id,
                "offset":offset,"max_bytes":crate::mail::MAX_BODY_BYTES});
            let chunk = self.message(MessageRoute::Body, request).await?.value;
            let head = first.get_or_insert_with(|| chunk.clone());
            ensure!(
                chunk["payload_digest"] == head["payload_digest"]
                    && chunk["cursor"] == head["cursor"],
                "message body changed between chunks"
            );
            body.push_str(chunk["body"].as_str().unwrap_or_default());
            ensure!(
                body.len() <= crate::mail::MAX_BODY_BYTES,
                "message body bound"
            );
            if chunk["complete"] == true {
                return Ok(Outcome {
                    value: json!({"message_id":message_id,"cursor":head["cursor"],
                        "payload_digest":head["payload_digest"],"body":body}),
                    exit_code: 0,
                });
            }
            ensure!(
                cursor(&chunk["next_offset"]) > cursor(&chunk["offset"]),
                "message body offset did not advance"
            );
            offset = chunk["next_offset"].as_str().unwrap_or_default().to_owned();
        }
        anyhow::bail!("message body chunk bound")
    }
}
/// Task comments (agent-comments v1). The thread and author come from the
/// credential's binding; nothing in the request names a task.
impl AgentClient {
    /// One send. Pass the same `operation_id` and body to retry after an
    /// uncertain outcome: the server returns the original comment.
    pub async fn comment_add(&self, operation_id: Uuid, body: &str) -> Result<Outcome> {
        let request = crate::comments::add_request(operation_id, body).map_err(|_| Failure {
            code: "invalid_comment_body",
            operation_id: Some(operation_id),
            effect: "not_applied",
            retryable: false,
        })?;
        let uncertain = Failure {
            // A replay with the same ID and body is safe, so say so.
            retryable: true,
            ..Failure::uncertain(Some(operation_id))
        };
        self.comment_exchange(
            crate::comments::ADD_PATH,
            request,
            Some(operation_id),
            uncertain,
        )
        .await
    }
    /// One page of the task's thread, newest first.
    pub async fn comment_list(&self, cursor: Option<&str>, limit: Option<u16>) -> Result<Outcome> {
        let request = crate::comments::list_request(cursor, limit).map_err(|_| Failure {
            code: "invalid_comment_page",
            operation_id: None,
            effect: "read_only",
            retryable: false,
        })?;
        self.comment_exchange(
            crate::comments::LIST_PATH,
            request,
            None,
            Failure::uncertain(None),
        )
        .await
    }
    async fn comment_exchange(
        &self,
        path: &str,
        request: Value,
        operation_id: Option<Uuid>,
        uncertain: Failure,
    ) -> Result<Outcome> {
        let now = Stamp::now()?;
        // Nothing is sent with an expired or malformed credential.
        self.credential(&now).map_err(|_| Failure {
            code: "authentication_or_authority_refused",
            operation_id,
            effect: "not_asserted",
            retryable: false,
        })?;
        let Ok((status, value)) = self
            .request(Method::POST, path, Some(canonical(&request)?), &now)
            .await
        else {
            return Err(uncertain.into());
        };
        let binding = &self.config.binding;
        if status == 200 {
            let associated = if operation_id.is_some() {
                crate::comments::added(binding, &request, &value)
            } else {
                crate::comments::page(binding, &request, &value)
            };
            return if associated {
                Ok(Outcome {
                    value,
                    exit_code: 0,
                })
            } else {
                Err(uncertain.into())
            };
        }
        // A published refusal is definite; anything else stays uncertain.
        if !valid("ErrorResponse", &value) || value["effect"] != "not_applied" {
            return Err(uncertain.into());
        }
        let exit_code = match status {
            401 | 403 => 14,
            409 => 11,
            400 | 404 | 426 => 12,
            _ => 13,
        };
        Ok(Outcome { value, exit_code })
    }
}
fn operation_http_status(status: u16, value: &Value) -> bool {
    matches!(
        (status, value["state"].as_str()),
        (200, Some("confirmed"))
            | (202, Some("pending" | "unknown"))
            | (409, Some("conflict"))
            | (422, Some("rejected"))
    )
}
fn operation_outcome(value: Value) -> Outcome {
    let exit_code = match value["state"].as_str() {
        Some("confirmed") => 0,
        Some("pending") => 10,
        Some("conflict") => 11,
        Some("rejected") => 12,
        _ => 13,
    };
    Outcome { value, exit_code }
}
fn remote_refusal(status: u16, value: Value, operation_id: Option<Uuid>) -> Result<Outcome> {
    if valid("ErrorResponse", &value) {
        // A refusal on this exchange does not erase an earlier uncertain send.
        if operation_id.is_some() {
            return Err(Failure {
                code: if matches!(status, 401 | 403) {
                    "authentication_or_authority_refused"
                } else {
                    "remote_operation_unresolved"
                },
                operation_id,
                effect: "unknown",
                retryable: false,
            }
            .into());
        }
        return Ok(Outcome {
            value,
            exit_code: if matches!(status, 401 | 403) { 14 } else { 15 },
        });
    }
    Err(Failure::uncertain(operation_id).into())
}

pub fn unknown_json(id: Uuid) -> Value {
    json!({"code":"remote_response_unknown","operation_id":id,"effect":"unknown","retryable":false})
}
