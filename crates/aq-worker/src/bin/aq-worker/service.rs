//! `aq-worker run --service-config` and `aq-worker service ...` (spec 0007 phase 1):
//! the glue between `aq-node-service` and the native pipeline.
mod enrollment;
mod lost_attempt;
mod recover_checkout;
mod recovery;
mod release_sandbox;
mod slot_attempt;
use lost_attempt::{LostAttempt, destroyable, lost_record, release_lost};
mod supervision;
use anyhow::{Context, Result, bail, ensure};
use aq_node_service::session::NodeSession;
use aq_node_service::{
    AqGitConfig, AqGitFactory, AttemptOutcome, EnrollmentFile, Layout, LinkError, MachineLink,
    RecoverOutcome, RepositoryBinding, ServerLink, ServiceConfig, ServiceState, SlotBackend,
    SlotContext, Supervisor, UnitSpec, default_machine_key, recorded_handoff, write_machine_json,
    write_private,
};
use aq_observation_store::{
    HttpOwnerPeer,
    node::{Node as ObservationNode, NodeLimits, Observations},
};
use aq_protocol::{
    EnrollRequest, HandoffNotice, HandoffReason, PROTOCOL_VERSION, SlotsRequest, valid_machine_key,
};
use aq_worker::{
    client::Client,
    config::WorkerConfig,
    native::journal::RecoveryHold,
    native::runner::{
        ClaudeDockerExecutor, ExecutorConfig, NativeConfig, NativeExecutor, RunDisposition,
        SalvageDisposition, claimed_attempts, destroy_claimed_sandboxes, retained_attempts,
        run_node_slot, run_until_attempt, salvage_interrupted,
    },
};
use clap::{Args, Subcommand};
use enrollment::enroll;
#[cfg(test)]
use enrollment::{Enrollment, Host, enroll_in, pending_enrollment, staged};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
pub(crate) use supervision::run_service;
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum ServiceCmd {
    /// Enroll node 1 with a single-use code (prompted, or the first stdin line), claim the machine
    /// key, mint nodes 2..N, write machine.json and service.toml. With --cf-service-token the next
    /// two prompts or lines are the node Access client id and secret.
    Enroll(ServiceEnrollArgs),
    /// Install and start the systemd user service for an enrolled profile.
    Install {
        #[arg(long)]
        profile: String,
    },
    /// Stop and remove the service; --purge also deletes its configuration and state.
    Uninstall {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        purge: bool,
    },
    Status {
        #[arg(long)]
        profile: String,
    },
    /// Record a failed handoff from a specific retained Git commit when the sandbox is absent.
    RecoverCheckout(recover_checkout::Checkout),
    /// Destroy the stopped sandbox an attempt of this slot preserved (its work was never
    /// copied back, so the service never destroys it). The reason is recorded as the destroy
    /// evidence.
    ReleaseSandbox(release_sandbox::ReleaseSandbox),
}
#[derive(Args)]
pub struct ServiceEnrollArgs {
    #[arg(long)]
    profile: String,
    #[arg(long)]
    server: String,
    /// Enrolled nodes on this machine. Agent slots are controlled by the server.
    #[arg(long = "nodes", alias = "slots", default_value_t = 1)]
    nodes: u32,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    machine_key: Option<String>,
    #[arg(long)]
    cf_service_token: bool,
    #[arg(long)]
    native_template: PathBuf,
    #[arg(long)]
    aq_git: PathBuf,
    #[arg(long, default_value = "/usr/bin/git")]
    git_executable: PathBuf,
    /// JSON array of RepositoryBinding to register before any offer (optional: a repository
    /// an offer names is registered from the offer when it is first seen).
    #[arg(long)]
    repositories: Option<PathBuf>,
    /// Enroll a profile that is already enrolled (after a revoke): redeems a new code and
    /// replaces its credentials and configuration. `service install` then restarts it.
    #[arg(long)]
    reconfigure: bool,
}

pub async fn dispatch(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Enroll(args) => enroll(args).await,
        ServiceCmd::Install { profile } => install(&profile),
        ServiceCmd::Uninstall { profile, purge } => uninstall(&profile, purge),
        ServiceCmd::Status { profile } => status(&profile),
        ServiceCmd::RecoverCheckout(checkout) => checkout.run().await,
        ServiceCmd::ReleaseSandbox(release) => release.run().await,
    }
}

/// The native pipeline behind the supervisor: one `run_until_attempt` per call.
pub struct NativeSlotBackend {
    template: PathBuf,
    conversation_root: Option<PathBuf>,
    /// Attempts whose restart handoff notice the server accepted; a retried recovery does
    /// not post them again.
    notified: std::sync::Mutex<std::collections::HashSet<Uuid>>,
    manual_pause: std::collections::BTreeMap<Uuid, watch::Receiver<bool>>,
    native_active: std::collections::BTreeMap<Uuid, Arc<std::sync::atomic::AtomicUsize>>,
    sessions: std::collections::BTreeMap<Uuid, Arc<NodeSession>>,
    /// Each node's observation registrar. The docker-local executor registers each
    /// launched attempt's transcript with it.
    observations: std::collections::BTreeMap<Uuid, Observations>,
    /// `[observations] transcript_max_bytes` and `transcript_max_files`.
    transcript_bound: Option<aq_handoff_note::TranscriptBound>,
    /// Spec 0017: each node's pulled admitted environments, shared by its slots.
    #[cfg(feature = "project_environments")]
    environments: std::collections::BTreeMap<Uuid, aq_environment_builder::node::Held>,
}
/// Starts a node's observation service: its store under `<state_root>/observations/<node id>`,
/// the transport owner handshake and delivery. A failure leaves the node running unobserved.
fn start_observations(
    config: &ServiceConfig,
    worker: &WorkerConfig,
    boot_id: Uuid,
) -> Option<ObservationNode> {
    let (o, node) = (&config.observations, worker.node_id);
    let ms = std::time::Duration::from_millis;
    let start = || -> Result<ObservationNode> {
        let edge: Vec<(&str, &str)> = match (&worker.cf_client_id, &worker.cf_client_secret) {
            (Some(id), Some(secret)) => vec![
                ("cf-access-client-id", id),
                ("cf-access-client-secret", secret),
            ],
            _ => vec![],
        };
        let peer = HttpOwnerPeer::for_node(
            &worker.server_url,
            node,
            &worker.credential,
            &edge,
            ms(o.request_timeout_ms),
        )?;
        let limits = NodeLimits {
            store_file_bytes: o.store_file_bytes,
            store_data_bytes: o.store_data_bytes,
            store_cache_bytes: o.store_cache_bytes as usize,
            tracked_identities: o.tracked_identities as usize,
            max_sources: u64::from(o.max_sources),
            resume_grace: ms(o.resume_grace_ms),
            discovery_entries: o.discovery_entries as usize,
            poll: ms(o.poll_ms),
            retry_start: ms(o.retry_start_ms),
            retry_max: ms(o.retry_max_ms),
            finish_drain: ms(o.finish_drain_ms),
        };
        let directory = config.state_root.join("observations");
        ObservationNode::start(
            &directory,
            node,
            boot_id,
            limits,
            peer,
            move |line| tracing::info!(%node, "{line}"),
        )
    };
    start()
        .inspect_err(|e| tracing::error!(%node, "node observations not started: {e:#}"))
        .ok()
}
/// Runs `f` on a scoped std thread, which is outside the async runtime: a blocking HTTP client
/// panics when built or dropped on a runtime thread. `None` when `f` panicked.
fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> Option<T> {
    std::thread::scope(|s| s.spawn(f).join().ok())
}
/// Stops each node's observation service and drops it (and its blocking HTTP client) off the
/// async runtime. Each `shutdown` waits for its thread, at most one server call.
fn stop_observations(nodes: Vec<ObservationNode>) {
    if off_runtime(move || nodes.into_iter().for_each(ObservationNode::shutdown)).is_none() {
        tracing::error!("node observations shutdown panicked");
    }
}
/// Starts the observation service of every enrolled node this service runs. A retired node
/// (an enrolled node above `nodes`) never registers this boot, so the server refuses its
/// Prepare forever: its retained observation data is left untouched, nothing is sent for it,
/// and one line per boot says so.
fn start_node_observations(
    config: &ServiceConfig,
    nodes: Vec<(WorkerConfig, Arc<NodeSession>)>,
) -> (
    Vec<ObservationNode>,
    std::collections::BTreeMap<Uuid, Observations>,
) {
    let (mut started, mut registrars) = (Vec::new(), std::collections::BTreeMap::new());
    for (worker, session) in nodes {
        let node = worker.node_id;
        if session.retired() {
            let retained = config
                .state_root
                .join("observations")
                .join(node.to_string());
            if retained.exists() {
                tracing::info!(%node, nodes = config.nodes, retained = %retained.display(),
                    "node observations not run: node is not run by this service; its retained observation data is left in place and not retried");
            }
            continue;
        }
        // Off the async runtime: a failed start drops a blocking HTTP client, which panics there.
        let service = std::thread::scope(|s| {
            s.spawn(|| start_observations(config, &worker, session.boot_id))
                .join()
                .ok()
                .flatten()
        });
        if let Some(service) = service {
            registrars.insert(node, service.observations());
            started.push(service);
        }
    }
    (started, registrars)
}
impl NativeSlotBackend {
    pub fn new(template: PathBuf) -> Self {
        Self {
            template,
            conversation_root: None,
            notified: Default::default(),
            manual_pause: Default::default(),
            native_active: Default::default(),
            sessions: Default::default(),
            observations: Default::default(),
            transcript_bound: None,
            #[cfg(feature = "project_environments")]
            environments: Default::default(),
        }
    }
    fn native_config(&self, ctx: &SlotContext) -> Result<NativeConfig> {
        #[cfg_attr(not(feature = "project_environments"), allow(unused_mut))]
        let mut cfg = slot_native_config(&self.template, ctx)?;
        #[cfg(feature = "project_environments")]
        if let (Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }), Some(held)) =
            (&mut cfg.executor, self.environments.get(&ctx.node_id))
        {
            settings.host.docker_local.environments = held.clone();
        }
        Ok(cfg)
    }
}
/// Recovery's `launch_closure_pending`: an execution was granted and the journal holds no
/// launch intent, host history or result for it (`runner::recovery::pre_launch`).
fn never_launched<T>(recovered: &Result<T>) -> bool {
    let hold = recovered.as_ref().err().and_then(|e| e.downcast_ref());
    matches!(hold, Some(RecoveryHold("launch_closure_pending", _)))
}
/// R1: a recovery that retrying cannot change (an unreadable or inconsistent journal, a
/// Prepare or launch whose disposition recovery cannot settle) holds the slot with its reason,
/// shown by `service status`, instead of being retried with backoff forever.
fn held_if_permanent(recovered: Result<RecoverOutcome>) -> Result<RecoverOutcome> {
    match recovered {
        Err(e) => match e.downcast_ref::<RecoveryHold>() {
            Some(RecoveryHold(reason, cause)) => {
                tracing::error!("recovery held: {reason}: {cause:#}");
                Ok(RecoverOutcome::Held(format!("{reason}: {cause:#}")))
            }
            None => Err(e),
        },
        ok => ok,
    }
}
/// The executor a node registers with. The template's own session host root is a placeholder:
/// every slot replaces it (`slot_native_config`) and an installer does not have to create it,
/// yet the executor's preflight opens it. A node whose template named a missing directory
/// never registered, so none of its slots polled. The preflight is given the service's runtime
/// root (the parent of each slot's runtime directory), which exists before any node starts.
fn registration_host_root(executor: &mut Option<ExecutorConfig>, ctx: &SlotContext) -> Result<()> {
    let runtime = ctx.git_socket.parent().and_then(Path::parent);
    if let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = executor {
        settings.host.root = runtime.context("slot runtime directory")?.to_owned();
    }
    Ok(())
}
/// The native template, pointed at the slot's state (journal, Git) and runtime directory
/// (aq-git's socket, the session host root). The renderer is the template's own.
fn slot_native_config(template: &Path, ctx: &SlotContext) -> Result<NativeConfig> {
    let mut cfg = NativeConfig::load(template)?;
    cfg.capacity.slots = 1;
    cfg.state_dir = ctx.state_dir.clone();
    cfg.git_socket = ctx.git_socket.clone();
    cfg.git_root = ctx.state_dir.join("git");
    if let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &mut cfg.executor {
        settings.host.root = ctx.host_root.clone();
    } else {
        bail!("the worker service needs the Claude docker-local executor in its native template");
    }
    cfg.validate()?;
    Ok(cfg)
}
/// D2: a recovery that failed because the guardian was lost: the outside proof that its
/// processes are dead, if one holds, and the reason. (The stopped contract carries only a
/// guardian's own evidence, so the stop itself is never reported.)
fn unproven<T>(result: &Result<T>) -> Option<(Option<aq_protocol::StopProof>, String)> {
    let e = result.as_ref().err()?;
    let lost = e.downcast_ref::<aq_session_host::StopUnproven>()?;
    Some((lost.proof.clone(), format!("{e:#}")))
}
/// Stops (never destroys) every sandbox of `attempt` that may still run; a stop must be
/// confirmed. The stopped sandbox stays for the operator (`service release-sandbox`).
async fn stop_attempt_sandboxes(
    backend: &mut impl aq_worker::backend::PlacementBackend,
    node: Uuid,
    attempt: Uuid,
) -> Result<()> {
    use aq_worker::backend::SandboxState;
    for entry in backend.list_owned(node).await? {
        let done = matches!(
            entry.state,
            SandboxState::Stopped | SandboxState::Archived | SandboxState::Destroyed
        );
        if entry.sandbox.owner.attempt_id == attempt && !done {
            let stopped = backend.stop(&entry.sandbox).await;
            ensure!(
                stopped.is_confirmed(),
                "sandbox {} stop unconfirmed",
                entry.sandbox.backend_id
            );
        }
    }
    Ok(())
}
/// The marker of an attempt that went through the lost path (`lost-<attempt>.json` in its
/// state dir): its sandbox is kept, never destroyed by the service.
/// What a restarted node tells the server about an interrupted attempt: the reason the service
/// recorded before the cut (a deliberate stop stays exempt from budgets), else a plain restart.
fn restart_reason(ctx: &SlotContext, attempt: Uuid) -> HandoffReason {
    recorded_handoff(&ctx.host_root, &ctx.state_dir, attempt).unwrap_or(HandoffReason::Restart)
}

#[async_trait::async_trait]
impl SlotBackend for NativeSlotBackend {
    async fn node_control(
        &self,
        ctx: SlotContext,
        reply: &aq_protocol::ControlResponse,
    ) -> Result<()> {
        let session = self
            .sessions
            .get(&ctx.node_id)
            .context("node session absent")?;
        session.update(reply)?;
        if !session.aware() {
            return Ok(());
        }
        session
            .register_current(|operation_id, slots| async move {
                let worker = WorkerConfig::load(&ctx.enrollment)?;
                let template = self.template.display();
                let mut config = NativeConfig::load(&self.template)
                    .with_context(|| format!("native template {template}"))?;
                registration_host_root(&mut config.executor, &ctx)?;
                ClaudeDockerExecutor::configured(&worker)?
                    .preflight(&config)
                    .with_context(|| format!("executor of native template {template}"))?;
                let client = aq_worker::native::http::NativeClient::from_config(&worker)?
                    .for_node_slot(1, slots);
                client
                    .register(&aq_admission::transport::Register {
                        schema_version: 1,
                        operation_id,
                        identity: aq_admission::transport::WorkerIdentity {
                            node_id: worker.node_id,
                            boot_id: session.boot_id,
                        },
                        runtime: config.runtime.runtime,
                        capabilities: config.capabilities,
                        features: config.features,
                    })
                    .await?;
                Ok(())
            })
            .await
    }

    async fn ready(&self, node_id: Uuid) {
        if let Some(session) = self.sessions.get(&node_id) {
            session.room().await;
        }
    }
    async fn run_attempt(
        &self,
        ctx: SlotContext,
        shutdown: watch::Receiver<bool>,
        pause: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome> {
        self.run_slot_attempt(ctx, shutdown, pause).await
    }
    async fn recover(&self, ctx: SlotContext) -> Result<RecoverOutcome> {
        held_if_permanent(Box::pin(self.recover_attempts(ctx)).await)
    }
    async fn release_claimed(&self, ctx: SlotContext) -> Result<usize> {
        let cfg = self.native_config(&ctx)?;
        let claimed = destroyable(&ctx.state_dir, claimed_attempts(&cfg, ctx.node_id)?);
        let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &cfg.executor
        else {
            return Ok(0);
        };
        if claimed.is_empty() {
            return Ok(0);
        }
        let docker = settings.host.docker_local.clone();
        let mut backend =
            aq_worker::backend::docker_local::DockerLocalBackend::connect_unix(docker).await?;
        let root = &settings.host.root;
        destroy_claimed_sandboxes(&mut backend, root, ctx.node_id, &claimed).await
    }
    fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>> {
        retained_attempts(&self.native_config(ctx)?, ctx.node_id)
    }
}

/// The outbox survives slot rotation and worker restart. An attempt may still
/// be settling when its snapshot appears, so retry until the server accepts it.
async fn upload_attempt_conversations(
    directory: PathBuf,
    link: Arc<ServerLink>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.wait_for(|v| *v) => break,
            _ = tick.tick() => {}
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(attempt) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| Uuid::parse_str(n).ok())
            else {
                continue;
            };
            let result = async {
                let body = aq_handoff_note::read_private(
                    &path,
                    aq_worker::conversation::MAX_SNAPSHOT_BYTES as u64,
                )?;
                let receipt = link
                    .attempt_conversation(attempt, &serde_json::from_slice(&body)?)
                    .await?;
                ensure!(
                    receipt["attempt_id"] == attempt.to_string(),
                    "unexpected attempt conversation acknowledgement"
                );
                std::fs::remove_file(path)?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(attempt_id = %attempt, %error, "provider conversation upload pending");
            }
            if *stop.borrow() {
                return;
            }
        }
    }
}

/// Spec 0017: the node's sandbox daemon as the admitted-environments poll uses it,
/// through the template's docker-local configuration (its static templates only).
#[cfg(feature = "project_environments")]
struct NodeTemplates(aq_worker::backend::docker_local::DockerLocalConfig);
#[cfg(feature = "project_environments")]
impl aq_environment_builder::node::Templates for NodeTemplates {
    async fn pull(
        &mut self,
        environment: &aq_environment_builder::node::AdmittedEnvironment,
    ) -> Result<()> {
        let mut docker = self.0.clone();
        docker.admit(std::slice::from_ref(environment))?;
        let digest = aq_worker::backend::TemplateDigest::parse(environment.digest.clone())?;
        let mut backend =
            aq_worker::backend::docker_local::DockerLocalBackend::connect_unix(docker).await?;
        Ok(backend.pull_template(&digest).await?)
    }
    async fn remove(
        &mut self,
        environment: &aq_environment_builder::node::AdmittedEnvironment,
    ) -> Result<aq_environment_builder::node::Removal> {
        let digest = aq_worker::backend::TemplateDigest::parse(environment.digest.clone())?;
        let mut backend =
            aq_worker::backend::docker_local::DockerLocalBackend::connect_unix(self.0.clone())
                .await?;
        Ok(backend.remove_template(&digest, &environment.image).await?)
    }
}
/// Spec 0017: pulls the server's admitted environments by digest, reports those
/// this node holds ready, and removes the images of those that left the list once
/// nothing uses them, on the node's idle presence interval.
#[cfg(feature = "project_environments")]
async fn hold_environments(
    worker: WorkerConfig,
    config: ServiceConfig,
    held: aq_environment_builder::node::Held,
    stop: watch::Receiver<bool>,
) {
    let node = worker.node_id;
    let native = match NativeConfig::load(&config.native_template) {
        Ok(native) => native,
        Err(e) => return tracing::error!(%node, "admitted environments not held: {e:#}"),
    };
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = native.executor
    else {
        return;
    };
    let server = aq_environment_builder::Server {
        url: worker.server_url,
        credential: worker.credential,
        cf_access: worker.cf_client_id.zip(worker.cf_client_secret),
        max_reply_bytes: config.environments.max_reply_bytes,
    };
    let poll = aq_environment_builder::node::Poll {
        // As often as an idle node renews its presence (`NodeInventory::poll_need`).
        interval: std::time::Duration::from_millis(native.inventory_ttl_ms / 2),
        max_list_bytes: config.environments.max_list_bytes,
    };
    let templates = NodeTemplates(settings.host.docker_local);
    if let Err(e) = aq_environment_builder::node::hold(&server, held, poll, stop, templates).await {
        tracing::error!(%node, "admitted environments not held: {e:#}");
    }
}
/// The node's one heartbeat. Only AQ's own refusal of the credential (revoked or invalid) stops
/// the node; any other error (an edge 401/403, a 5xx AQ did not mark transient, a protocol
/// refusal) is logged and the loop is started again after a jittered back-off.
async fn node_heartbeat(
    worker: WorkerConfig,
    node_stop: watch::Sender<bool>,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    use aq_worker::client::CallError;
    let node = worker.node_id;
    let client = Client::from_config(&worker).inspect_err(|e| {
        tracing::error!(%node, "node stopped: its heartbeat client was not built: {e:#}");
        node_stop.send_replace(true);
    })?;
    let mut attempt = 0;
    loop {
        let (mut wait, mut ended) = (stop.clone(), stop.clone());
        let started = tokio::time::Instant::now();
        let stopped = async move { drop(wait.wait_for(|v| *v).await) };
        match aq_worker::client::run_loop(&client, stopped).await {
            Ok(()) => return Ok(()),
            Err(e @ (CallError::Revoked | CallError::InvalidCredential)) => {
                tracing::error!(%node, "node stopped: the server refused its credential: {e}");
                node_stop.send_replace(true);
                return Err(e.into());
            }
            Err(e) => {
                if started.elapsed() > std::time::Duration::from_secs(120) {
                    attempt = 0;
                }
                let delay = aq_node_service::backoff::delay(attempt, &mut rand::thread_rng());
                tracing::warn!(%node, attempt, "heartbeat failed, retrying in {delay:?}: {e}");
                attempt += 1;
                tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = ended.wait_for(|v| *v) => return Ok(()) }
            }
        }
    }
}
/// The enrolled profile's configuration and the layout it describes (its own state and runtime
/// roots, not the defaults).
fn loaded(profile: &str) -> Result<(Layout, ServiceConfig)> {
    loaded_in(Layout::for_profile(profile)?, profile)
}
fn loaded_in(defaults: Layout, profile: &str) -> Result<(Layout, ServiceConfig)> {
    let config = ServiceConfig::load(&defaults.service_toml()).with_context(|| {
        format!("profile {profile} is not enrolled; run `aq-worker service enroll` first")
    })?;
    Ok((Layout::for_service(&defaults.config_dir, &config), config))
}
fn install(profile: &str) -> Result<()> {
    let (layout, config) = loaded(profile)?;
    let host = aq_node_service::os_service::RealHost;
    let pre = aq_node_service::preflight(&host)?;
    let manager = aq_node_service::detect()?;
    let sandbox_unit = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .is_some_and(|c| c.join("systemd/user/aq-sandbox-podman.service").exists());
    manager.install(&UnitSpec {
        profile: profile.to_owned(),
        aq_worker: std::fs::canonicalize(std::env::current_exe()?)?,
        service_toml: layout.service_toml(),
        requires_sandbox_unit: sandbox_unit,
        stop_timeout_secs: config.stop_timeout_secs(),
    })?;
    println!("installed aq-node@{profile} ({})", manager.kind());
    if !pre.linger {
        println!(
            "the service stops when you log out; run `loginctl enable-linger $USER` to keep it running"
        );
    }
    Ok(())
}
fn uninstall(profile: &str, purge: bool) -> Result<()> {
    // A custom state_root (or runtime_root) in service.toml is purged too.
    let layout = match loaded(profile) {
        Ok((layout, _)) => layout,
        Err(_) => Layout::for_profile(profile)?,
    };
    let manager = aq_node_service::detect()?;
    if manager.status(profile)? == ServiceState::NotInstalled {
        println!("aq-node@{profile}: not installed");
    } else {
        manager.uninstall(profile)?;
        println!("aq-node@{profile}: removed");
    }
    if purge {
        purge_dirs(&layout)?;
        println!("configuration and state purged");
    }
    Ok(())
}
fn purge_dirs(layout: &Layout) -> Result<()> {
    for dir in [&layout.state_root, &layout.runtime_root, &layout.config_dir] {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", dir.display())),
        }
    }
    Ok(())
}
fn status(profile: &str) -> Result<()> {
    let manager = aq_node_service::detect()?;
    let state = match manager.status(profile)? {
        ServiceState::Active => "active".to_owned(),
        ServiceState::Inactive => "inactive".to_owned(),
        ServiceState::Failed => "failed".to_owned(),
        ServiceState::NotInstalled => "not installed".to_owned(),
        ServiceState::Other(s) => s,
    };
    println!("aq-node@{profile} ({}): {state}", manager.kind());
    if let Ok((layout, config)) = loaded(profile) {
        println!("machine {}: {} node(s)", config.machine_key, config.nodes);
        for node in layout.enrolled_nodes() {
            let view = layout.for_node(node);
            let id = EnrollmentFile::load(&view.slot_enrollment(1))
                .map(|e| e.node_id.to_string())
                .unwrap_or_else(|_| "unenrolled".into());
            println!(
                "node {node}: {id}{}",
                if node > config.nodes {
                    " (retiring)"
                } else {
                    ""
                }
            );
            for slot in view.existing_slots() {
                println!("  {}", slot_line(&view, slot));
            }
        }
    }
    Ok(())
}
/// One slot of `service status`: held with its reason, or not held.
fn slot_line(layout: &Layout, slot: u32) -> String {
    match layout.held_reason(slot) {
        Some(why) => format!(
            "slot {slot}: held until the owner releases it: {}",
            why.trim()
        ),
        None => format!("slot {slot}: not held"),
    }
}
/// `service release-sandbox`: the operator release of a sandbox one of the slot's attempts
/// preserved, through the slot's own node and the profile's native template.
/// Whether the slot's recovery still holds `attempt` for salvage: its `current` journal retains
/// it and it did not go through the lost path. An unreadable journal (a running service owns the
/// state) counts as held.
fn held_for_salvage(
    current: &Path,
    attempt: Uuid,
    retained: impl FnOnce() -> Result<Vec<Uuid>>,
) -> bool {
    current.join("journal").exists()
        && !lost_record(current, attempt).exists()
        && retained().map_or(true, |r| r.contains(&attempt))
}

#[cfg(test)]
mod tests;
