//! Worker-owned bounded protected allocation and credential issuance. Nothing
//! here grants provider access, a lease, launch authority or a right to delete.
use crate::{
    client::{AgentEgressCredential, ClientConfig, Http, SessionCredential},
    context::{ContextBundle, MAX_BUNDLE_BYTES, StartupContext},
    files::PrivateDir,
    node::NodeClient,
};
use anyhow::{Context as _, Result, ensure};
use aq_agent_api::{Binding, Context, digest, parse};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
};
use uuid::Uuid;
const MIB: u64 = 1024 * 1024;
const NODE_JOURNAL_BYTES: usize = 1024 * 1024;
const NODE_RESERVATION: u64 = 2 * MIB;
const SESSION_RESERVATION: u64 = 64 * MIB;
const MAX_INTENTS: usize = 16;
const CREDENTIAL_BYTES: usize = 1024 * 1024;
/// Includes two atomic-replacement journal copies. Slots remain allocated until
/// a future explicit recovery/retention protocol authorizes reclamation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_sessions: u32,
    pub total_reserved_bytes: u64,
    pub session_reserved_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: 16,
            total_reserved_bytes: NODE_RESERVATION + 16 * SESSION_RESERVATION,
            session_reserved_bytes: SESSION_RESERVATION,
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.max_sessions)
                && self.session_reserved_bytes >= SESSION_RESERVATION
                && self.total_reserved_bytes
                    >= NODE_RESERVATION
                        .checked_add(self.session_reserved_bytes)
                        .context("reservation overflow")?,
            "allocation limits below conservative reservation"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IssuanceState {
    Prepared,
    Unknown,
    Ready { credential_version: u64 },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    id: Uuid,
    expected_version: u64,
    request_digest: String,
    state: IssuanceState,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allocation {
    binding: Binding,
    startup_digest: String,
    origin: String,
    egress_digest: Option<String>,
    intents: Vec<Intent>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format_version: u32,
    node_id: Uuid,
    limits: Limits,
    sessions: BTreeMap<Uuid, Allocation>,
}
/// One node owner serializes provisioning. Agent operation journals use separate
/// per-session locks and credentials are replaced atomically under this owner.
pub struct Allocator {
    directory: PrivateDir,
    _owner: File,
    state: State,
    poisoned: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationStatus {
    pub session_id: Uuid,
    pub intent_id: Uuid,
    pub expected_version: u64,
    pub state: IssuanceState,
    pub reserved_bytes: u64,
}
/// The ready session files the node hands to the runner for installation in
/// the sandbox. The bytes include the agent token and are never printed.
pub struct SessionDelivery {
    pub client: Vec<u8>,
    pub context: Vec<u8>,
    pub credential: Vec<u8>,
}
impl std::fmt::Debug for SessionDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionDelivery { <redacted> }")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    pub binding: Binding,
    pub expires_at_ms: u64,
}
fn credential_expiry_ms(context: &serde_json::Value) -> Result<u64> {
    let at = context["credential"]["expires_at"]
        .as_str()
        .context("credential expiry absent")?;
    let ms = chrono::DateTime::parse_from_rfc3339(at)
        .map_err(|_| anyhow::anyhow!("credential expiry invalid"))?
        .timestamp_millis();
    u64::try_from(ms).map_err(|_| anyhow::anyhow!("credential expiry invalid"))
}
/// Install node-delivered session files into the agent's private state
/// directory. `endpoint` replaces the server origin (the runner's loopback AQ
/// proxy), Access egress material is refused, and the credential must expire
/// after `now_ms` and no later than `not_after_ms` (the attempt grant). The
/// server remains authoritative for expiry and revocation.
pub fn install_delivered(
    state_dir: &Path,
    delivery: &SessionDelivery,
    endpoint: &str,
    not_after_ms: u64,
    now_ms: u64,
) -> Result<Installed> {
    let mut config: ClientConfig = serde_json::from_value(parse(&delivery.client)?)?;
    ensure!(
        config.format_version == 1,
        "delivered client configuration version"
    );
    ensure!(
        !config.edge_authentication,
        "sandbox delivery must not carry Access egress credentials"
    );
    config.binding.validate()?;
    let bundle: ContextBundle = serde_json::from_value(parse_context(&delivery.context)?)?;
    bundle.validate()?;
    ensure!(
        bundle.binding == config.binding,
        "delivered context binding mismatch"
    );
    ensure!(
        delivery.credential.len() <= aq_agent_api::MAX_DOCUMENT_BYTES,
        "credential file bound"
    );
    let issued: SessionCredential = serde_json::from_value(parse(&delivery.credential)?)
        .map_err(|_| anyhow::anyhow!("delivered credential shape refused"))?;
    ensure!(
        issued.api_version == 1
            && issued.token.starts_with("aqa_")
            && (36..=4096).contains(&issued.token.len())
            && !issued.token.chars().any(char::is_whitespace),
        "delivered credential shape refused"
    );
    Context::new(issued.context.clone(), &config.binding)?;
    ensure!(
        bundle.graph_context == issued.context,
        "delivered context and credential differ"
    );
    let expires_at_ms = credential_expiry_ms(&issued.context)?;
    ensure!(
        expires_at_ms > now_ms,
        "delivered credential already expired"
    );
    ensure!(
        expires_at_ms <= not_after_ms,
        "delivered credential outlives the attempt grant"
    );
    Http::new(endpoint)?;
    config.server_url = endpoint.to_owned();
    let dir = PrivateDir::create(state_dir)?;
    dir.write("context.json", &delivery.context)?;
    dir.write("client.json", &serde_json::to_vec(&config)?)?;
    dir.write("credential.json", &serde_json::to_vec(&issued)?)?;
    Ok(Installed {
        path: dir.path,
        binding: config.binding,
        expires_at_ms,
    })
}
/// Remove the installed agent credential. Idempotent.
pub fn revoke_delivered(state_dir: &Path) -> Result<()> {
    match PrivateDir::open(state_dir) {
        Ok(dir) => dir.remove("credential.json").map(|_| ()),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}
fn parse_context(bytes: &[u8]) -> Result<serde_json::Value> {
    ensure!(bytes.len() <= MAX_BUNDLE_BYTES, "context byte bound");
    let aq_agent_api::StrictValue(value) = serde_json::from_slice(bytes)?;
    Ok(value)
}
fn startup_digest(value: &StartupContext) -> Result<String> {
    digest(&serde_json::to_value(value)?).map_err(Into::into)
}
fn edge_digest(value: Option<&AgentEgressCredential>) -> Result<Option<String>> {
    value
        .map(|v| {
            ensure!(
                !v.client_id.is_empty()
                    && !v.client_secret.is_empty()
                    && v.client_id.len() <= 4096
                    && v.client_secret.len() <= 4096,
                "agent egress bound"
            );
            reqwest::header::HeaderValue::from_str(&v.client_id)
                .map_err(|_| anyhow::anyhow!("invalid protected agent egress header"))?;
            reqwest::header::HeaderValue::from_str(&v.client_secret)
                .map_err(|_| anyhow::anyhow!("invalid protected agent egress header"))?;
            digest(&serde_json::to_value(v)?).map_err(Into::into)
        })
        .transpose()
}
fn intent_digest(a: &Allocation, expected: u64) -> Result<String> {
    digest(&serde_json::json!({"binding":a.binding,"startup_digest":a.startup_digest,"origin":a.origin,"egress_digest":a.egress_digest,"expected_credential_version":expected})).map_err(Into::into)
}
impl Allocator {
    pub fn open(root: &Path, node_id: Uuid, limits: Limits) -> Result<Self> {
        ensure!(!node_id.is_nil(), "nil allocation node");
        limits.validate()?;
        let directory = PrivateDir::create(root)?;
        let owner = directory.lock()?;
        let state = match directory.optional("allocations.json", NODE_JOURNAL_BYTES)? {
            Some(b) => serde_json::from_value(parse(&b)?)?,
            None => State {
                format_version: 1,
                node_id,
                limits: limits.clone(),
                sessions: BTreeMap::new(),
            },
        };
        let value = Self {
            directory,
            _owner: owner,
            state,
            poisoned: false,
        };
        ensure!(
            value.state.format_version == 1
                && value.state.node_id == node_id
                && value.state.limits == limits,
            "allocation owner/version/limits changed"
        );
        value.validate_state()?;
        value.check_root()?;
        Ok(value)
    }
    fn validate_state(&self) -> Result<()> {
        ensure!(
            self.state.sessions.len() <= self.state.limits.max_sessions as usize,
            "allocation slot bound"
        );
        ensure!(
            self.reserved_bytes()? <= self.state.limits.total_reserved_bytes,
            "allocation byte bound"
        );
        let mut ids = BTreeSet::new();
        for (session, a) in &self.state.sessions {
            a.binding.validate()?;
            ensure!(
                *session == a.binding.session_id
                    && a.binding.node_id == self.state.node_id
                    && !a.intents.is_empty()
                    && a.intents.len() <= MAX_INTENTS,
                "allocation binding/history bound"
            );
            Http::new(&a.origin)?;
            for pair in a.intents.windows(2) {
                let permitted = match pair[0].state {
                    IssuanceState::Ready { credential_version } => {
                        pair[1].expected_version == credential_version
                    }
                    IssuanceState::Unknown => {
                        pair[1].expected_version == pair[0].expected_version
                            || Some(pair[1].expected_version)
                                == pair[0].expected_version.checked_add(1)
                    }
                    IssuanceState::Prepared => false,
                };
                ensure!(permitted, "corrupt issuance version history");
            }
            for intent in &a.intents {
                ensure!(
                    !intent.id.is_nil()
                        && ids.insert(intent.id)
                        && intent.request_digest == intent_digest(a, intent.expected_version)?,
                    "issuance intent identity/payload conflict"
                );
                if let IssuanceState::Ready { credential_version } = intent.state {
                    ensure!(
                        intent.expected_version.checked_add(1) == Some(credential_version),
                        "installed version mismatch"
                    );
                }
            }
        }
        Ok(())
    }
    pub fn reserved_bytes(&self) -> Result<u64> {
        self.state
            .limits
            .session_reserved_bytes
            .checked_mul(self.state.sessions.len() as u64)
            .and_then(|n| n.checked_add(NODE_RESERVATION))
            .context("allocation reservation overflow")
    }
    fn check_root(&self) -> Result<()> {
        for member in std::fs::read_dir(&self.directory.path)? {
            let member = member?;
            let name = member.file_name();
            let name = name.to_str().context("unexpected allocation member")?;
            if matches!(name, "owner.lock" | "allocations.json") {
                continue;
            }
            let id: Uuid = name
                .parse()
                .context("unexpected allocation member; preserve and reconcile")?;
            ensure!(
                self.state.sessions.contains_key(&id) && member.file_type()?.is_dir(),
                "untracked allocation member; preserve and reconcile"
            );
        }
        Ok(())
    }
    fn commit<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        ensure!(
            !self.poisoned,
            "allocation persistence uncertain; reopen required"
        );
        self.check_root()?;
        let mut next = self.state.clone();
        let result = change(&mut next)?;
        let bytes = serde_json::to_vec(&next)?;
        ensure!(
            bytes.len() <= NODE_JOURNAL_BYTES,
            "node allocation journal full"
        );
        self.poisoned = true;
        self.directory.write("allocations.json", &bytes)?;
        self.state = next;
        self.poisoned = false;
        Ok(result)
    }
    fn directory(&self, session: Uuid) -> Result<PrivateDir> {
        ensure!(
            self.state.sessions.contains_key(&session),
            "allocation absent"
        );
        let dir = PrivateDir::create(&self.directory.path.join(session.to_string()))?;
        for entry in std::fs::read_dir(&dir.path)? {
            let entry = entry?;
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0
                    && metadata.nlink() == 1,
                "private member ownership/type refused"
            );
            ensure!(
                matches!(
                    entry.file_name().to_str(),
                    Some(
                        "owner.lock"
                            | "operations.json"
                            | "context.json"
                            | "client.json"
                            | "credential.json"
                            | "agent-egress.json"
                    )
                ),
                "unexpected private member; preserve and reconcile before further writes"
            );
        }
        Ok(dir)
    }
    pub fn status(&self, session: Uuid) -> Result<AllocationStatus> {
        let a = self
            .state
            .sessions
            .get(&session)
            .context("allocation absent")?;
        let intent = a.intents.last().context("issuance intent absent")?;
        Ok(AllocationStatus {
            session_id: session,
            intent_id: intent.id,
            expected_version: intent.expected_version,
            state: intent.state.clone(),
            reserved_bytes: self.state.limits.session_reserved_bytes,
        })
    }
    /// Reserve and persist a tokenless startup before any issuance exchange. A
    /// repeated UUID must describe exactly the same immutable request.
    pub fn allocate(
        &mut self,
        node: &NodeClient,
        startup: StartupContext,
        intent_id: Uuid,
        expected_version: u64,
        edge: Option<AgentEgressCredential>,
    ) -> Result<AllocationStatus> {
        startup.validate()?;
        ensure!(
            !intent_id.is_nil()
                && expected_version < u64::MAX
                && node.node_id == self.state.node_id
                && startup.binding.node_id == node.node_id,
            "issuance owner/version refused"
        );
        node.distinct_agent_egress(edge.as_ref())?;
        let edge_digest = edge_digest(edge.as_ref())?;
        Http::new(&node.origin)?;
        let session = startup.binding.session_id;
        let mut a = Allocation {
            binding: startup.binding.clone(),
            startup_digest: startup_digest(&startup)?,
            origin: node.origin.clone(),
            egress_digest: edge_digest,
            intents: vec![],
        };
        let request_digest = intent_digest(&a, expected_version)?;
        if let Some(old) = self.state.sessions.get(&session) {
            ensure!(
                old.binding == a.binding
                    && old.startup_digest == a.startup_digest
                    && old.origin == a.origin
                    && old.egress_digest == a.egress_digest
                    && old.intents.iter().any(|i| i.id == intent_id
                        && i.expected_version == expected_version
                        && i.request_digest == request_digest),
                "allocation or intent reused with changed payload"
            );
        } else {
            ensure!(
                !self
                    .state
                    .sessions
                    .values()
                    .any(|a| a.intents.iter().any(|i| i.id == intent_id)),
                "issuance UUID belongs to another allocation"
            );
            a.intents.push(Intent {
                id: intent_id,
                expected_version,
                request_digest,
                state: IssuanceState::Prepared,
            });
            let available = fs2::available_space(&self.directory.path)?;
            self.commit(|s| {
                ensure!(
                    s.sessions.len() < s.limits.max_sessions as usize,
                    "node session allocations full; retained slots require explicit recovery"
                );
                let required = (s.sessions.len() as u64 + 1)
                    .checked_mul(s.limits.session_reserved_bytes)
                    .and_then(|n| n.checked_add(NODE_RESERVATION))
                    .context("reservation overflow")?;
                ensure!(
                    required <= s.limits.total_reserved_bytes && required <= available,
                    "node allocation bytes or physical headroom full"
                );
                s.sessions.insert(session, a);
                Ok(())
            })?;
        }
        let dir = self.directory(session)?;
        if dir.optional("context.json", MAX_BUNDLE_BYTES)?.is_none() {
            dir.write("context.json", &serde_json::to_vec(&startup)?)?;
        } else {
            self.startup(session)?;
        }
        if let Some(edge) = edge {
            let bytes = serde_json::to_vec(&edge)?;
            ensure!(bytes.len() <= 16_384, "agent egress file bound");
            dir.write("agent-egress.json", &bytes)?;
        }
        self.status(session)
    }
    fn startup(&self, session: Uuid) -> Result<StartupContext> {
        let a = self
            .state
            .sessions
            .get(&session)
            .context("allocation absent")?;
        let dir = self.directory(session)?;
        let bytes = dir.read("context.json", MAX_BUNDLE_BYTES)?;
        let value = parse_context(&bytes)?;
        let startup = match serde_json::from_value::<StartupContext>(value.clone()) {
            Ok(v) => v,
            Err(_) => {
                let bundle: ContextBundle = serde_json::from_value(value)?;
                bundle.validate()?;
                StartupContext {
                    binding: bundle.binding,
                    profiled_offer: bundle.profiled_offer,
                    contents: bundle.contents,
                }
            }
        };
        startup.validate()?;
        ensure!(
            startup.binding == a.binding && startup_digest(&startup)? == a.startup_digest,
            "startup allocation payload changed"
        );
        Ok(startup)
    }
    /// Explicit caller intent authorizes exactly one new CAS attempt. For unknown
    /// issuance the caller explicitly chooses the pre-commit or post-commit CAS
    /// version with a NEW intent; replaying the old uncertain intent is forbidden.
    /// The daemon remains the live version/authority fence.
    pub fn prepare_rotation(
        &mut self,
        node: &NodeClient,
        session: Uuid,
        intent_id: Uuid,
        expected_version: u64,
    ) -> Result<AllocationStatus> {
        ensure!(
            !intent_id.is_nil() && expected_version < u64::MAX,
            "rotation intent/version refused"
        );
        self.startup(session)?;
        let a = self
            .state
            .sessions
            .get(&session)
            .context("allocation absent")?;
        ensure!(
            node.node_id == self.state.node_id && node.origin == a.origin,
            "rotation node/origin changed"
        );
        let request_digest = intent_digest(a, expected_version)?;
        if let Some(old) = a.intents.iter().find(|i| i.id == intent_id) {
            ensure!(
                old.expected_version == expected_version && old.request_digest == request_digest,
                "rotation UUID payload conflict"
            );
            return self.status(session);
        }
        ensure!(
            !self
                .state
                .sessions
                .values()
                .any(|a| a.intents.iter().any(|i| i.id == intent_id)),
            "rotation UUID belongs to another allocation"
        );
        let old = a.intents.last().unwrap();
        let permitted = match old.state {
            IssuanceState::Ready { credential_version } => expected_version == credential_version,
            // An unknown exchange may have failed either before or after the
            // daemon committed. A new explicit intent may CAS either possible
            // version; the daemon, never this local guess, selects the live one.
            IssuanceState::Unknown => {
                expected_version == old.expected_version
                    || Some(expected_version) == old.expected_version.checked_add(1)
            }
            IssuanceState::Prepared => {
                anyhow::bail!("prepared issuance must be resolved before rotation")
            }
        };
        ensure!(
            permitted,
            "explicit rotation must name a possible live CAS version"
        );
        self.commit(|s| {
            let a = s.sessions.get_mut(&session).unwrap();
            ensure!(
                a.intents.len() < MAX_INTENTS,
                "issuance history full; retain for explicit recovery"
            );
            a.intents.push(Intent {
                id: intent_id,
                expected_version,
                request_digest,
                state: IssuanceState::Prepared,
            });
            Ok(())
        })?;
        self.status(session)
    }
    fn retained_credential(
        &self,
        session: Uuid,
        expected: u64,
    ) -> Result<Option<SessionCredential>> {
        let dir = self.directory(session)?;
        let Some(bytes) = dir.optional("credential.json", CREDENTIAL_BYTES)? else {
            return Ok(None);
        };
        let issued: SessionCredential = serde_json::from_value(parse(&bytes)?)?;
        let a = self.state.sessions.get(&session).unwrap();
        Context::new(issued.context.clone(), &a.binding)?;
        ensure!(
            issued.api_version == 1
                && issued.token.starts_with("aqa_")
                && (36..=4096).contains(&issued.token.len())
                && !issued.token.chars().any(char::is_whitespace),
            "retained credential shape refused"
        );
        let version = issued.context["credential"]["version"]
            .as_str()
            .and_then(|v| v.parse::<u64>().ok())
            .context("credential version absent")?;
        if Some(version) != expected.checked_add(1) {
            return Ok(None);
        }
        Ok(Some(issued))
    }
    fn install(
        &mut self,
        session: Uuid,
        intent_id: Uuid,
        issued: SessionCredential,
    ) -> Result<PathBuf> {
        let a = self
            .state
            .sessions
            .get(&session)
            .context("allocation absent")?
            .clone();
        let intent = a.intents.last().unwrap();
        ensure!(intent.id == intent_id, "stale issuance intent");
        let version = intent
            .expected_version
            .checked_add(1)
            .context("credential version overflow")?;
        Context::new(issued.context.clone(), &a.binding)?;
        ensure!(
            issued.context["credential"]["version"].as_str() == Some(version.to_string().as_str()),
            "credential install version mismatch"
        );
        let bundle = self
            .startup(session)?
            .with_graph_context(issued.context.clone())?;
        let dir = self.directory(session)?;
        let credential = serde_json::to_vec(&issued)?;
        ensure!(
            credential.len() <= CREDENTIAL_BYTES,
            "credential file bound"
        );
        dir.write("credential.json", &credential)?;
        dir.write("context.json", &serde_json::to_vec(&bundle)?)?;
        let config = ClientConfig {
            format_version: 1,
            server_url: a.origin,
            binding: a.binding,
            edge_authentication: a.egress_digest.is_some(),
        };
        dir.write("client.json", &serde_json::to_vec(&config)?)?;
        self.commit(|s| {
            let i = s
                .sessions
                .get_mut(&session)
                .unwrap()
                .intents
                .last_mut()
                .unwrap();
            ensure!(i.id == intent_id, "stale issuance installation");
            i.state = IssuanceState::Ready {
                credential_version: version,
            };
            Ok(())
        })?;
        Ok(dir.path)
    }
    /// Local-only recovery can finish an already retained exact-version response;
    /// it never sends HTTP, recreates plaintext or changes an unknown CAS request.
    pub fn recover_local(&mut self, session: Uuid) -> Result<Option<PathBuf>> {
        let status = self.status(session)?;
        if let Some(issued) = self.retained_credential(session, status.expected_version)? {
            return self.install(session, status.intent_id, issued).map(Some);
        }
        ensure!(
            !matches!(status.state, IssuanceState::Ready { .. }),
            "ready credential missing or changed; preserve allocation"
        );
        Ok(None)
    }
    /// The ready session files for node-to-runner delivery.
    pub fn delivery(&self, session: Uuid) -> Result<SessionDelivery> {
        let dir = PrivateDir::open(&self.ready_path(session)?)?;
        Ok(SessionDelivery {
            client: dir.read("client.json", 16_384)?,
            context: dir.read("context.json", MAX_BUNDLE_BYTES)?,
            credential: dir.read("credential.json", CREDENTIAL_BYTES)?,
        })
    }
    pub fn ready_path(&self, session: Uuid) -> Result<PathBuf> {
        ensure!(
            matches!(self.status(session)?.state, IssuanceState::Ready { .. }),
            "session provisioning not ready"
        );
        self.startup(session)?;
        let issued = self
            .retained_credential(session, self.status(session)?.expected_version)?
            .context("installed credential missing or changed")?;
        let allocation = self.state.sessions.get(&session).unwrap();
        let dir = self.directory(session)?;
        let config: ClientConfig =
            serde_json::from_value(parse(&dir.read("client.json", 16_384)?)?)?;
        ensure!(
            config.format_version == 1
                && config.binding == allocation.binding
                && config.server_url == allocation.origin
                && config.edge_authentication == allocation.egress_digest.is_some(),
            "published client configuration changed"
        );
        let bundle: ContextBundle =
            serde_json::from_value(parse_context(&dir.read("context.json", MAX_BUNDLE_BYTES)?)?)?;
        bundle.validate()?;
        ensure!(
            bundle.graph_context == issued.context,
            "published context and credential differ"
        );
        let edge = dir
            .optional("agent-egress.json", 16_384)?
            .map(|b| {
                serde_json::from_value::<AgentEgressCredential>(parse(&b)?)
                    .map_err(anyhow::Error::from)
            })
            .transpose()?;
        ensure!(
            edge_digest(edge.as_ref())? == allocation.egress_digest,
            "published agent egress changed"
        );
        Ok(dir.path)
    }
    /// At most one exchange for each persisted intent. Any error leaves Unknown.
    pub async fn issue_once(
        &mut self,
        node: &NodeClient,
        session: Uuid,
        intent_id: Uuid,
    ) -> Result<PathBuf> {
        let status = self.status(session)?;
        ensure!(status.intent_id == intent_id, "stale issuance intent");
        if matches!(status.state, IssuanceState::Ready { .. }) {
            return self.ready_path(session);
        }
        if status.state == IssuanceState::Unknown {
            if let Some(path) = self.recover_local(session)? {
                return Ok(path);
            }
            anyhow::bail!(
                "issuance outcome unknown; plaintext cannot be replayed; explicit rotation required"
            );
        }
        let startup = self.startup(session)?;
        let a = self.state.sessions.get(&session).unwrap();
        ensure!(
            node.node_id == self.state.node_id && node.origin == a.origin,
            "issuance node/origin changed"
        );
        let dir = self.directory(session)?;
        let edge = dir
            .optional("agent-egress.json", 16_384)?
            .map(|b| {
                serde_json::from_value::<AgentEgressCredential>(parse(&b)?)
                    .map_err(anyhow::Error::from)
            })
            .transpose()?;
        ensure!(
            edge_digest(edge.as_ref())? == a.egress_digest,
            "agent egress payload changed"
        );
        node.distinct_agent_egress(edge.as_ref())?;
        self.commit(|s| {
            s.sessions
                .get_mut(&session)
                .unwrap()
                .intents
                .last_mut()
                .unwrap()
                .state = IssuanceState::Unknown;
            Ok(())
        })?;
        let issued = node
            .issue(&startup.binding, status.expected_version)
            .await?;
        self.install(session, intent_id, issued)
    }
}
