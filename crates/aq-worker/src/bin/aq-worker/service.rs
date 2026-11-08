//! `aq-worker run --service-config` and `aq-worker service ...` (spec 0007 phase 1):
//! the glue between `aq-node-service` and the native pipeline.
use anyhow::{Context, Result, bail, ensure};
use aq_node_service::{
    AqGitConfig, AqGitFactory, AttemptOutcome, EnrollmentFile, Layout, LinkError, MachineLink,
    MeasurementConfig, RecoverOutcome, RepositoryBinding, ServerLink, ServiceConfig, ServiceState,
    SlotBackend, SlotContext, Supervisor, UnitSpec, default_machine_key, recorded_handoff,
    write_machine_json, write_private,
};
use aq_protocol::{
    ArtifactPin, EnrollRequest, HandoffNotice, HandoffReason, MeasurementSubmission,
    PROTOCOL_VERSION, SlotsRequest, valid_machine_key,
};
use aq_worker::{
    client::Client,
    config::WorkerConfig,
    native::runner::{
        ClaudeDockerExecutor, ExecutorConfig, NativeConfig, RunDisposition, SalvageDisposition,
        release_claimed_sandboxes, retained_attempts, run_until_attempt, salvage_interrupted,
    },
};
use clap::{Args, Subcommand};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum ServiceCmd {
    /// Enroll slot 1 with a single-use code (prompted, or the first stdin line), claim the machine
    /// key, mint slots 2..N, write machine.json and service.toml. With --cf-service-token the next
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
    /// Destroy the stopped sandbox an attempt of this slot preserved (its work was never
    /// copied back, so the service never destroys it). The reason is recorded as the destroy
    /// evidence.
    ReleaseSandbox {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        slot: u32,
        #[arg(long)]
        attempt_id: Uuid,
        #[arg(long)]
        reason: String,
    },
    /// Run the qualification measurement now and submit it.
    Measure {
        #[arg(long)]
        profile: String,
        /// Write the outputs to <DIR>/measure (for `ms1-profiled.py` `measured()`) and do
        /// not submit.
        #[arg(long, hide = true)]
        output: Option<PathBuf>,
    },
}
#[derive(Args)]
pub struct ServiceEnrollArgs {
    #[arg(long)]
    profile: String,
    #[arg(long)]
    server: String,
    #[arg(long, default_value_t = 1)]
    slots: u32,
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
    /// JSON array of RepositoryBinding.
    #[arg(long)]
    repositories: PathBuf,
    #[arg(long)]
    measurement_token_file: Option<PathBuf>,
    #[arg(long, requires = "measurement_token_file")]
    measurement_model: Option<String>,
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
        ServiceCmd::Measure { profile, output } => measure(&profile, output.as_deref()).await,
        ServiceCmd::ReleaseSandbox {
            profile,
            slot,
            attempt_id,
            reason,
        } => release_sandbox(&profile, slot, attempt_id, &reason).await,
    }
}

/// The native pipeline behind the supervisor: one `run_until_attempt` per call.
pub struct NativeSlotBackend {
    template: PathBuf,
    /// Slot 1's node: it owns the measurement sandbox.
    measure_node: Uuid,
    /// Attempts whose restart handoff notice the server accepted; a retried recovery does
    /// not post them again.
    notified: std::sync::Mutex<std::collections::HashSet<Uuid>>,
}
impl NativeSlotBackend {
    pub fn new(template: PathBuf, measure_node: Uuid) -> Self {
        Self {
            template,
            measure_node,
            notified: Default::default(),
        }
    }
    fn native_config(&self, ctx: &SlotContext) -> Result<NativeConfig> {
        slot_native_config(&self.template, ctx)
    }
}
/// The native template, pointed at the slot's state (journal, Git) and runtime directory
/// (aq-git's socket, the session host root), with the slot's renderer.
fn slot_native_config(template: &Path, ctx: &SlotContext) -> Result<NativeConfig> {
    let mut cfg = NativeConfig::load(template)?;
    cfg.state_dir = ctx.state_dir.clone();
    cfg.git_socket = ctx.git_socket.clone();
    cfg.git_root = ctx.state_dir.join("git");
    if let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &mut cfg.executor {
        settings.host.root = ctx.host_root.clone();
        if let Some(pin) = &ctx.renderer {
            settings.host.renderer = to_artifact_ref(pin)?;
        }
    } else {
        bail!("the worker service needs the Claude docker-local executor in its native template");
    }
    if let Some(pin) = &ctx.renderer {
        cfg.renderer = to_artifact_ref(pin)?;
    }
    cfg.validate()?;
    Ok(cfg)
}
/// Converts a wire pin to the pipeline's artifact reference (same JSON shape).
fn to_artifact_ref<T: serde::de::DeserializeOwned>(pin: &ArtifactPin) -> Result<T> {
    serde_json::from_value(serde_json::to_value(pin)?).context("invalid renderer pin")
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
/// D2 ruling C: a lost attempt with an outside stop proof. Its sandboxes are stopped (kept),
/// then the server is asked to fail it and retry its task (`LostAttemptRelease`). On success
/// `lost-<attempt>.json` in `state_dir` records that its unsaved work was not salvaged, and the
/// slot is freed (`Lost`); otherwise the slot is held for the operator.
async fn release_lost(
    stop: impl AsyncFnOnce() -> Result<()>,
    post: impl AsyncFnOnce(aq_protocol::LostAttemptRelease) -> Result<aq_protocol::LostAttemptReleased>,
    state_dir: &Path,
    attempt: Uuid,
    proof: aq_protocol::StopProof,
    why: &str,
) -> RecoverOutcome {
    if let Err(e) = stop().await {
        let why = format!("the lost attempt's sandbox could not be stopped: {e:#}; {why}");
        tracing::error!(%attempt, "recovery held: {why}");
        return RecoverOutcome::Held(why);
    }
    let request = aq_protocol::LostAttemptRelease {
        schema_version: 1,
        attempt_id: attempt,
        proof: proof.clone(),
    };
    let released = match post(request).await {
        Ok(released) => released,
        Err(e) => {
            let why = format!("the server refused the lost-attempt release: {e:#}; {why}");
            tracing::error!(%attempt, "recovery held: {why}");
            return RecoverOutcome::Held(why);
        }
    };
    let record = serde_json::json!({"schema_version": 1, "attempt_id": attempt, "proof": proof,
        "reason": released.reason, "charged": released.charged, "released": released.released,
        "held": released.held, "salvaged": false,
        "sandbox": "stopped and kept; release it with `aq-worker service release-sandbox`"});
    let path = state_dir.join(format!("lost-{attempt}.json"));
    if let Err(e) = write_private(&path, record.to_string().as_bytes()) {
        tracing::warn!(%attempt, "lost-attempt record not written: {e:#}");
    }
    tracing::warn!(%attempt, ?proof, reason = ?released.reason, charged = released.charged,
        retried = released.released, held = ?released.held,
        "lost attempt released: its unsaved work was not salvaged; its sandbox is kept for release-sandbox");
    RecoverOutcome::Lost
}
/// What a restarted node tells the server about an interrupted attempt: the reason the service
/// recorded before the cut (a deliberate stop stays exempt from budgets), else a plain restart.
fn restart_reason(ctx: &SlotContext, attempt: Uuid) -> HandoffReason {
    recorded_handoff(&ctx.host_root, &ctx.state_dir, attempt).unwrap_or(HandoffReason::Restart)
}

#[async_trait::async_trait]
impl SlotBackend for NativeSlotBackend {
    async fn run_attempt(
        &self,
        ctx: SlotContext,
        shutdown: watch::Receiver<bool>,
        pause: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome> {
        let worker = WorkerConfig::load(&ctx.enrollment)?;
        let cfg = self.native_config(&ctx)?;
        let mut executor = ClaudeDockerExecutor::configured(&worker)?;
        Ok(
            match run_until_attempt(&worker, cfg, &mut executor, shutdown, pause).await? {
                RunDisposition::AttemptStopped => AttemptOutcome::AttemptStopped,
                RunDisposition::PollingPaused => AttemptOutcome::Paused,
                RunDisposition::ShutdownBeforeAdmission | RunDisposition::NoOffer => {
                    AttemptOutcome::ShutdownBeforeAdmission
                }
            },
        )
    }
    async fn recover(&self, ctx: SlotContext) -> Result<RecoverOutcome> {
        let worker = WorkerConfig::load(&ctx.enrollment)?;
        let cfg = self.native_config(&ctx)?;
        let link = ServerLink::from_enrollment(&EnrollmentFile::load(&ctx.enrollment)?)?;
        let mut outcome = RecoverOutcome::Clean;
        for attempt in retained_attempts(&cfg, worker.node_id)? {
            let notice = HandoffNotice {
                schema_version: 1,
                attempt_id: attempt,
                reason: restart_reason(&ctx, attempt),
            };
            if !self.notified.lock().unwrap().contains(&attempt) {
                match link.handoff(&notice).await {
                    Ok(()) => drop(self.notified.lock().unwrap().insert(attempt)),
                    Err(e) => tracing::warn!(%attempt, "handoff notice: {e:#}"),
                }
            }
            let salvaged = salvage_interrupted(&worker, &cfg, attempt).await;
            if let Some((proof, why)) = unproven(&salvaged) {
                let Some(proof) = proof else {
                    tracing::error!(%attempt, "recovery held: {why}");
                    return Ok(RecoverOutcome::Held(why));
                };
                let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
                    &cfg.executor
                else {
                    return Ok(RecoverOutcome::Held(why));
                };
                let docker = settings.host.docker_local.clone();
                let stop = async || {
                    let mut backend =
                        aq_worker::backend::docker_local::DockerLocalBackend::connect_unix(docker)
                            .await?;
                    stop_attempt_sandboxes(&mut backend, worker.node_id, attempt).await
                };
                let post = async |r| link.lost_attempt(&r).await;
                match release_lost(stop, post, &ctx.state_dir, attempt, proof, &why).await {
                    RecoverOutcome::Lost => outcome = RecoverOutcome::Lost,
                    held => return Ok(held),
                }
                continue;
            }
            match salvaged? {
                SalvageDisposition::Unsalvageable(why) => {
                    return Ok(RecoverOutcome::Held(why.to_owned()));
                }
                SalvageDisposition::Salvaged => outcome = RecoverOutcome::Salvaged,
                SalvageDisposition::AlreadyReported | SalvageDisposition::NotStarted => {
                    if outcome == RecoverOutcome::Clean {
                        outcome = RecoverOutcome::Reported;
                    }
                }
            }
        }
        Ok(outcome)
    }
    async fn release_claimed(&self, ctx: SlotContext) -> Result<usize> {
        release_claimed_sandboxes(&self.native_config(&ctx)?, ctx.node_id).await
    }
    fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>> {
        retained_attempts(&self.native_config(ctx)?, ctx.node_id)
    }
    async fn measure(
        &self,
        token_file: &Path,
        model: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<MeasurementSubmission> {
        let node = self.measure_node;
        let m = crate::measure::run(&self.template, token_file, model, node, cancel).await?;
        Ok(m.measured.submission)
    }
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|r| r.to_lowercase().contains("microsoft"))
}

pub async fn enroll(args: ServiceEnrollArgs) -> Result<()> {
    let layout = Layout::for_profile(&args.profile)?;
    let cf = args.cf_service_token;
    let secrets = move |code: bool| match code {
        true => crate::read_secrets(cf, false).map(|s| Enrollment {
            code: s.code,
            cf: s.cf,
        }),
        false => crate::read_access().map(|access| Enrollment {
            code: String::new(),
            cf: Some(access),
        }),
    };
    enroll_in(&args, &layout, crate::hostname(), is_wsl(), secrets).await
}
/// What the operator types (or pipes) at enrollment: the code, or (resuming with
/// `--reconfigure --cf-service-token`) only the Access client id and secret.
struct Enrollment {
    code: String,
    cf: Option<(String, String)>,
}
/// Slot 1's credential after the code is redeemed and before the machine key claim succeeds.
/// A rerun resumes the claim from it instead of redeeming a new code.
fn pending_enrollment(layout: &Layout) -> PathBuf {
    layout.config_dir.join("enroll-pending.toml")
}

fn staged(path: &Path) -> PathBuf {
    path.with_extension("toml.new")
}
/// The slot file an earlier run of this enrollment saved (staged or installed), when it is the
/// server's node for that slot.
fn saved_slot(layout: &Layout, slot: u32, node_id: Uuid, server: &str) -> Option<EnrollmentFile> {
    let path = layout.slot_enrollment(slot);
    [staged(&path), path]
        .iter()
        .filter_map(|p| EnrollmentFile::load(p).ok())
        .find(|f| f.node_id == node_id && f.server_url == server)
}

/// Enrollment, in this order: every input is checked before a secret is read or the server
/// is called; an enrolled profile is refused without `--reconfigure`; an unfinished one resumes
/// at the claim; slot 1's file is written only once the machine key claim succeeds.
async fn enroll_in(
    args: &ServiceEnrollArgs,
    layout: &Layout,
    hostname: Option<String>,
    wsl: bool,
    secrets: impl FnOnce(bool) -> Result<Enrollment>,
) -> Result<()> {
    let machine_key = match &args.machine_key {
        Some(key) => {
            ensure!(
                valid_machine_key(key),
                "machine key {key:?} is not valid: 1-63 characters of a-z, 0-9 and '-', not starting with '-'"
            );
            key.clone()
        }
        None => hostname
            .as_deref()
            .and_then(default_machine_key)
            .context("pass --machine-key: the host name does not make a valid machine key")?,
    };
    ensure!(
        (1..=aq_protocol::MAX_MACHINE_SLOTS).contains(&args.slots),
        "slots must be 1..={}",
        aq_protocol::MAX_MACHINE_SLOTS
    );
    aq_node_service::checked_origin(&args.server)?;
    let name = args
        .name
        .clone()
        .or(hostname)
        .context("cannot determine hostname; pass --name")?;
    let repositories: Vec<RepositoryBinding> = serde_json::from_slice(
        &std::fs::read(&args.repositories)
            .with_context(|| format!("read {}", args.repositories.display()))?,
    )
    .context("repositories must be a JSON array of repository bindings")?;
    let template = std::fs::read(&args.native_template)
        .with_context(|| format!("read {}", args.native_template.display()))?;
    let native = layout.config_dir.join("native.json");
    let mut config = ServiceConfig {
        schema_version: 1,
        profile: args.profile.clone(),
        machine_key: machine_key.clone(),
        slots: args.slots,
        native_template: native.clone(),
        state_root: layout.state_root.clone(),
        runtime_root: None,
        aq_git: AqGitConfig {
            binary: std::fs::canonicalize(&args.aq_git)
                .with_context(|| format!("aq-git {}", args.aq_git.display()))?,
            git_executable: args.git_executable.clone(),
        },
        repositories,
        measurement: MeasurementConfig {
            enabled: args.measurement_token_file.is_some(),
            token_file: args.measurement_token_file.clone(),
            model: args.measurement_model.clone(),
            ..MeasurementConfig::default()
        },
        shutdown_grace_ms: 120_000,
    };
    // Repository count and format, absolute paths (the token file too), bounds, and the
    // socket paths under the runtime root.
    config.validate()?;

    let pending = pending_enrollment(layout);
    let first = if pending.exists() {
        let mut first = EnrollmentFile::load(&pending)?;
        ensure!(
            first.server_url == args.server,
            "an unfinished enrollment for {} is in {}; pass that --server, or delete the file to start over with a new code",
            first.server_url,
            pending.display()
        );
        println!("resuming the unfinished enrollment at the machine key claim; no code is needed");
        if args.reconfigure && args.cf_service_token {
            let (id, secret) = secrets(false)?.cf.context("Access credentials")?;
            (first.cf_client_id, first.cf_client_secret) = (Some(id), Some(secret));
            first.save(&pending)?;
        }
        first
    } else {
        if layout.service_toml().exists() && !args.reconfigure {
            bail!(
                "profile {} is already enrolled; nothing was changed and no code was used. To enroll it again (for example after a revoke), pass --reconfigure",
                args.profile
            );
        }
        let typed = secrets(true)?;
        let (cf_id, cf_secret) = typed.cf.map_or((None, None), |(i, s)| (Some(i), Some(s)));
        let client = Client::new(&args.server, None, cf_id.clone(), cf_secret.clone())?;
        let enrolled = client
            .enroll(&EnrollRequest {
                code: typed.code,
                name: name.clone(),
                worker_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION,
            })
            .await?;
        let first = EnrollmentFile {
            server_url: args.server.clone(),
            node_id: enrolled.node_id,
            credential: enrolled.credential,
            cf_client_id: cf_id,
            cf_client_secret: cf_secret,
        };
        first.save(&pending)?;
        first
    };
    // Claim the machine key and mint the siblings, always, even with one slot.
    let minted = ServerLink::from_enrollment(&first)?
        .slots(&SlotsRequest { schema_version: 1, machine_key: machine_key.clone(), slots: args.slots })
        .await
        .map_err(|e| match e.downcast_ref::<LinkError>() {
            Some(l) if l.status == 409 => anyhow::anyhow!("machine key {machine_key} is in use by another machine; revoke it from the Swarm page or pass --machine-key (rerun this command: the code is not needed again)"),
            Some(l) if l.revoked() => anyhow::anyhow!("the server refused the unfinished enrollment's credential; delete {} and enroll with a new code", pending.display()),
            Some(l) if l.edge_refused() => anyhow::anyhow!("Cloudflare Access refused the stored token at the machine key claim; run enroll with --reconfigure --cf-service-token and new Access credentials (the code is not needed again)"),
            _ => e.context("the machine key claim failed; rerun this command to resume it (the code is not needed again)"),
        })?;
    let mut files = vec![first.clone()];
    for slot in 2..=args.slots {
        let entry = minted
            .slots
            .iter()
            .find(|s| s.slot == slot)
            .with_context(|| format!("the server did not return slot {slot}"))?;
        let file = match &entry.credential {
            Some(credential) => EnrollmentFile {
                node_id: entry.node_id,
                credential: credential.clone(),
                ..first.clone()
            },
            // Minted by an earlier run of this enrollment: the server returns a credential
            // only once, so it must be the one that run saved here.
            None => saved_slot(layout, slot, entry.node_id, &first.server_url).with_context(|| {
                format!(
                    "slot {slot} was minted by an earlier run of this enrollment, which stopped before it saved the slot's credential; it cannot be recovered. Revoke the machine from the Swarm page, delete {}, then run enroll with --reconfigure and a new code",
                    pending.display()
                )
            })?,
        };
        files.push(file);
    }
    // Every slot file is staged beside its final path first, then all are renamed. A cut
    // during the renames leaves some staged and some installed files; the rerun (pending is
    // still there) finds each one, staged or installed, and finishes the renames.
    for (slot, file) in (1..).zip(&files) {
        file.save(&staged(&layout.slot_enrollment(slot)))?;
    }
    for slot in 1..=args.slots {
        let path = layout.slot_enrollment(slot);
        std::fs::rename(staged(&path), &path)
            .with_context(|| format!("install {}", path.display()))?;
    }
    // The machine's label on the Swarm page is its key unless --name gives one.
    let label = args.name.as_deref().unwrap_or(&machine_key);
    write_machine_json(&layout.machine_json(), &machine_key, label, wsl)?;
    write_private(&native, &template)?;
    config.native_template = std::fs::canonicalize(&native)?;
    config.save(&layout.service_toml())?;
    std::fs::remove_file(&pending).context("remove the pending enrollment")?;
    println!(
        "machine {machine_key}: slots {}",
        files
            .iter()
            .map(|f| f.node_id.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    if args.reconfigure {
        println!(
            "run `aq-worker service install --profile {}` to restart the service with this enrollment",
            args.profile
        );
    }
    Ok(())
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
    if let Ok((_, config)) = loaded(profile) {
        println!("machine {}: {} slot(s)", config.machine_key, config.slots);
    }
    Ok(())
}
async fn measure(profile: &str, output: Option<&Path>) -> Result<()> {
    let (layout, config) = loaded(profile)?;
    let m = &config.measurement;
    let (Some(file), Some(model)) = (&m.token_file, &m.model) else {
        bail!("measurement is not configured for profile {profile}");
    };
    let slot1 = EnrollmentFile::load(&layout.slot_enrollment(1))?;
    // One measurement at a time: the service skips its own while this holds the lock.
    let lock = match aq_node_service::measure::measure_lock(&layout.measure_lock(), false)? {
        Some(lock) => lock,
        None => {
            println!(
                "a measurement for profile {profile} is already running (the service started it); waiting for it to finish"
            );
            let path = layout.measure_lock();
            tokio::task::spawn_blocking(move || aq_node_service::measure::measure_lock(&path, true))
                .await??
                .context("measurement lock")?
        }
    };
    let (tx, cancel) = watch::channel(false);
    let watcher = tokio::spawn(async move {
        crate::shutdown_signal().await;
        let _ = tx.send(true);
    });
    let done =
        crate::measure::run(&config.native_template, file, model, slot1.node_id, cancel).await;
    watcher.abort();
    drop(lock);
    let done = done?;
    for (call, total, out) in &done.measured.totals {
        println!("{call}: total_input={total} output={out}");
    }
    println!(
        "capacity_tokens={} observed_at_ms={}",
        done.measured.capacity_tokens, done.measured.observed_at_ms
    );
    if let Some(dir) = output {
        aq_node_service::measure::write_output(dir, &done.files)?;
        println!(
            "written to {}; not submitted",
            dir.join("measure").display()
        );
        return Ok(());
    }
    let pin = ServerLink::from_enrollment(&slot1)?
        .submit_measurement(&done.measured.submission)
        .await?;
    println!(
        "qualification accepted: effective at {} ms, until {} ms",
        pin.effective_at_ms, pin.expires_at_ms
    );
    Ok(())
}

/// `service release-sandbox`: the operator release of a sandbox one of the slot's attempts
/// preserved, through the slot's own node and the profile's native template.
async fn release_sandbox(profile: &str, slot: u32, attempt: Uuid, reason: &str) -> Result<()> {
    let (layout, config) = loaded(profile)?;
    ensure!(
        (1..=config.slots).contains(&slot),
        "profile {profile} has slots 1..={}",
        config.slots
    );
    let node = EnrollmentFile::load(&layout.slot_enrollment(slot))?.node_id;
    // Only the executor settings matter here; the slot's state directory stands in for the
    // attempt's own (which may be archived).
    let state_dir = std::fs::canonicalize(layout.slot_state(slot))?;
    let ctx = SlotContext {
        slot,
        node_id: node,
        enrollment: layout.slot_enrollment(slot),
        git_socket: layout.git_socket(slot),
        host_root: state_dir.join("host"),
        state_dir,
        renderer: None,
    };
    let cfg = slot_native_config(&config.native_template, &ctx)?;
    let released =
        aq_worker::native::runner::release_preserved_sandbox(&cfg, node, attempt, reason).await?;
    println!(
        "operator release: attempt {attempt} sandbox(es) {} destroyed; reason recorded as destroy evidence",
        released.join(", ")
    );
    Ok(())
}

/// D2, proof (b): before any slot starts a process, check that the unit's cgroup holds only
/// this invocation; then a lost guardian older than this process is known dead. The proof is
/// named when such a slot is held (the stopped contract cannot carry it yet).
fn attest_prior_invocation() {
    let attested = aq_node_service::invocation::prior_invocation_gone().and_then(|gone| {
        if let Some((invocation, unit)) = &gone {
            let own = aq_session_host::ownership::process_evidence(std::process::id())?;
            aq_session_host::lost_guardian::attest_prior_invocation_gone(
                own.start_ticks,
                invocation.clone(),
                unit.clone(),
            )?;
        }
        Ok(gone)
    });
    match attested {
        Ok(Some((invocation, unit))) => tracing::info!(
            %invocation, %unit,
            "the unit's cgroup holds no process of a previous invocation"
        ),
        Ok(None) => tracing::info!(
            "not under systemd, or the unit's cgroup holds other processes: no teardown proof"
        ),
        Err(e) => tracing::warn!("previous invocation check: {e:#}"),
    }
}

/// `aq-worker run --service-config`: supervise every slot until SIGTERM or ctrl-c.
pub async fn run_service(service_config: &Path) -> Result<()> {
    let config = ServiceConfig::load(service_config)?;
    let config_dir = service_config
        .parent()
        .context("service config has no directory")?;
    let layout = Layout::for_service(config_dir, &config);
    // Every socket lives under the runtime root (D1): create and check it before any slot.
    layout.prepare_runtime()?;
    attest_prior_invocation();
    let slot1 = EnrollmentFile::load(&layout.slot_enrollment(1))?;
    let link = Arc::new(ServerLink::from_enrollment(&slot1)?);
    // A measurement cut short (kill, crash, reboot) left its sandbox: release it before work.
    let lock = layout.measure_lock();
    match crate::measure::sweep(&config.native_template, slot1.node_id, &lock).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("released {n} leftover measurement sandbox(es)"),
        Err(e) => tracing::warn!("measurement sandbox sweep: {e:#}"),
    }
    let (tx, rx) = watch::channel(false);
    let watcher = tokio::spawn(async move {
        crate::shutdown_signal().await;
        let _ = tx.send(true);
    });
    // Only slot 1 reports; the server groups the sibling slots by the machine key.
    let machine =
        aq_machine_load::sampler_from_file(&layout.machine_json(), layout.state_root.clone()).map(
            |(sampler, interval)| {
                let (report_link, mut stop) = (link.clone(), rx.clone());
                tokio::spawn(aq_machine_load::report_loop(
                    sampler,
                    interval,
                    move |report| {
                        let link = report_link.clone();
                        async move {
                            link.machine_report(&report)
                                .await
                                .map_err(|e| e.to_string())
                        }
                    },
                    async move {
                        while !*stop.borrow() {
                            if stop.changed().await.is_err() {
                                break;
                            }
                        }
                    },
                ))
            },
        );
    tracing::info!(profile = %config.profile, machine = %config.machine_key, slots = config.slots,
        runtime = %layout.runtime_root.display(), "worker service starting");
    // Each slot's handoff notices go out under that slot's own credential (D4).
    let control = Arc::new(MachineLink::new(link.clone(), &layout, config.slots));
    let backend = Arc::new(NativeSlotBackend::new(
        config.native_template.clone(),
        slot1.node_id,
    ));
    let git = Arc::new(AqGitFactory(config.aq_git.clone()));
    let result = Supervisor::new(config, layout, backend, control, git)
        .run(rx)
        .await;
    watcher.abort();
    if let Some(machine) = machine {
        machine.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_ctx(dir: &Path) -> SlotContext {
        SlotContext {
            slot: 1,
            node_id: Uuid::nil(),
            enrollment: dir.join("worker.toml"),
            state_dir: dir.join("current"),
            host_root: dir.join("run/1/host"),
            git_socket: dir.join("run/1/git.sock"),
            renderer: None,
        }
    }
    #[test]
    fn restart_reason_follows_the_handoff_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = slot_ctx(dir.path());
        let attempt = Uuid::from_u128(9);
        let driver = ctx.host_root.join("driver-1");
        std::fs::create_dir_all(&driver).unwrap();
        std::fs::write(
            driver.join("driver.json"),
            serde_json::json!({"owner": {"attempt_id": attempt}}).to_string(),
        )
        .unwrap();
        assert_eq!(restart_reason(&ctx, attempt), HandoffReason::Restart);
        for (written, expected) in [
            ("service_stop", HandoffReason::ServiceStop),
            ("owner_stop", HandoffReason::OwnerStop),
        ] {
            let _ = std::fs::remove_file(driver.join("handoff"));
            std::fs::write(
                driver.join("handoff"),
                format!(r#"{{"schema_version":1,"reason":"{written}"}}"#),
            )
            .unwrap();
            assert_eq!(restart_reason(&ctx, attempt), expected);
        }
        assert_eq!(
            restart_reason(&ctx, Uuid::from_u128(10)),
            HandoffReason::Restart
        );
    }
    #[test]
    fn renderer_pin_converts_to_an_artifact_ref() {
        let pin = ArtifactPin {
            id: "aq-terminal-renderer".into(),
            version: "0.1.0".into(),
            digest: "a".repeat(64),
        };
        let r: aq_session_protocol::ArtifactRef = to_artifact_ref(&pin).unwrap();
        assert_eq!(r.digest.as_str(), "a".repeat(64));
        assert!(
            to_artifact_ref::<aq_session_protocol::ArtifactRef>(&ArtifactPin {
                digest: "x".into(),
                ..pin
            })
            .is_err()
        );
    }

    /// A loopback stand-in for aq-server's enroll and slots routes. The slots route answers
    /// 409 for the first `refuse_claims` calls.
    struct FakeServer {
        origin: String,
        enrolls: Arc<std::sync::atomic::AtomicU32>,
        claims: Arc<std::sync::atomic::AtomicU32>,
        _server: tokio::task::AbortHandle,
    }
    async fn fake_server(refuse_claims: u32) -> FakeServer {
        fake_server_with(refuse_claims, None).await
    }
    /// Like aq-server: a slot's credential is returned only by the claim that minted it (one
    /// per slot-1 credential here). With `access`, the slots route sits behind an edge that
    /// answers a bare 403 unless that Access client id is sent.
    async fn fake_server_with(refuse_claims: u32, access: Option<&'static str>) -> FakeServer {
        use axum::{
            Json, Router, http::HeaderMap, http::StatusCode, response::IntoResponse, routing::post,
        };
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        let (enrolls, claims) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (e, c) = (enrolls.clone(), claims.clone());
        let minted = Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        let app = Router::new()
            .route("/v1/worker/enroll", post(move || async move {
                let n = e.fetch_add(1, SeqCst) + 1;
                Json(serde_json::json!({"node_id": Uuid::from_u128(n.into()),
                    "credential": format!("aqn_cred{n}"), "heartbeat_interval_secs": 30}))
            }))
            .route("/v1/worker/slots", post(move |h: HeaderMap| async move {
                let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                if let Some(id) = access && header("cf-access-client-id") != id {
                    return (StatusCode::FORBIDDEN, "<html>Forbidden</html>").into_response();
                }
                if c.fetch_add(1, SeqCst) < refuse_claims {
                    let body = serde_json::json!({"error": "conflict", "message": "in use"});
                    return (StatusCode::CONFLICT, Json(body)).into_response();
                }
                let new = minted.lock().unwrap().insert(header("authorization"));
                let credential = new.then_some("aqn_slot2");
                let body = serde_json::json!({"schema_version": 1, "machine_key": "desk", "slots": [
                    {"slot": 2, "node_id": Uuid::from_u128(99), "credential": credential}]});
                (StatusCode::OK, Json(body)).into_response()
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeServer {
            origin,
            enrolls,
            claims,
            _server: server.abort_handle(),
        }
    }
    struct Profile {
        dir: tempfile::TempDir,
        layout: Layout,
    }
    fn profile() -> Profile {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("repositories.json"), b"[]").unwrap();
        std::fs::write(dir.path().join("native.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("aq-git"), b"").unwrap();
        let layout = Layout {
            config_dir: dir.path().join("cfg"),
            state_root: dir.path().join("state"),
            runtime_root: dir.path().join("run/local"),
        };
        Profile { dir, layout }
    }
    impl Profile {
        fn args(&self, server: &str) -> ServiceEnrollArgs {
            let at = |name: &str| self.dir.path().join(name);
            ServiceEnrollArgs {
                profile: "local".into(),
                server: server.into(),
                slots: 2,
                name: Some("Desk".into()),
                machine_key: Some("desk".into()),
                cf_service_token: false,
                native_template: at("native.json"),
                aq_git: at("aq-git"),
                git_executable: "/usr/bin/git".into(),
                repositories: at("repositories.json"),
                measurement_token_file: None,
                measurement_model: None,
                reconfigure: false,
            }
        }
        async fn enroll(&self, args: &ServiceEnrollArgs, code: Option<&str>) -> Result<()> {
            let code = code.map(str::to_owned);
            let secrets = move |wanted: bool| {
                assert!(wanted, "only the code is typed here");
                let code = code.expect("no secret may be read here");
                Ok(Enrollment { code, cf: None })
            };
            enroll_in(args, &self.layout, Some("desk".into()), false, secrets).await
        }
        /// A resumed enrollment that types only new Access credentials.
        async fn enroll_access(&self, args: &ServiceEnrollArgs, id: &str) -> Result<()> {
            let id = id.to_owned();
            let secrets = move |wanted: bool| {
                assert!(!wanted, "a resume needs no code");
                Ok(Enrollment {
                    code: String::new(),
                    cf: Some((id, "new-secret".into())),
                })
            };
            enroll_in(args, &self.layout, Some("desk".into()), false, secrets).await
        }
        /// Slot 1's redeemed credential, saved before the claim (as a cut run leaves it).
        fn pending(&self, server: &str) -> EnrollmentFile {
            let first = EnrollmentFile {
                server_url: server.into(),
                node_id: Uuid::from_u128(1),
                credential: "aqn_cred1".into(),
                cf_client_id: Some("old-id".into()),
                cf_client_secret: Some("old-secret".into()),
            };
            first.save(&pending_enrollment(&self.layout)).unwrap();
            first
        }
        fn slot1(&self) -> String {
            std::fs::read_to_string(self.layout.slot_enrollment(1)).unwrap_or_default()
        }
    }
    fn count(n: &std::sync::atomic::AtomicU32) -> u32 {
        n.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Review #5 and #6: every input is checked before a secret is read or the server is
    /// called, the plaintext rule of the node link included.
    #[tokio::test]
    async fn enroll_validates_everything_before_a_secret_or_a_request() {
        let server = fake_server(0).await;
        let p = profile();
        let repo = |format: &str| {
            serde_json::json!({"repository_id": Uuid::nil(), "identity": "r",
                "binding_generation": 1, "object_format": format})
        };
        let write_repos = |repos: Vec<serde_json::Value>| {
            std::fs::write(
                p.dir.path().join("repositories.json"),
                serde_json::to_vec(&repos).unwrap(),
            )
            .unwrap()
        };
        let mut relative = p.args(&server.origin);
        relative.measurement_token_file = Some("token".into());
        relative.measurement_model = Some("m".into());
        let e = p.enroll(&relative, None).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("token_file must be an absolute path"),
            "{e:#}"
        );
        write_repos(vec![repo("sha1"); 17]);
        let e = p.enroll(&p.args(&server.origin), None).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("at most 16 repositories"),
            "{e:#}"
        );
        write_repos(vec![repo("sha256")]);
        let e = p.enroll(&p.args(&server.origin), None).await.unwrap_err();
        assert!(format!("{e:#}").contains("object_format"), "{e:#}");
        write_repos(vec![repo("sha1")]);
        for origin in ["http://10.0.0.5:8081", "http://aq.example.com"] {
            let e = p.enroll(&p.args(origin), None).await.unwrap_err();
            assert!(format!("{e:#}").contains("must be https"), "{e:#}");
        }
        assert_eq!((count(&server.enrolls), count(&server.claims)), (0, 0));
        assert!(!p.layout.config_dir.exists(), "nothing written");
    }

    /// Review #5: an enrolled profile is refused (no code is read) unless --reconfigure.
    #[tokio::test]
    async fn enroll_refuses_an_enrolled_profile_without_reconfigure() {
        let server = fake_server(0).await;
        let p = profile();
        let mut args = p.args(&server.origin);
        p.enroll(&args, Some("code-1")).await.unwrap();
        assert!(p.slot1().contains("aqn_cred1") && p.layout.service_toml().exists());
        let e = p.enroll(&args, None).await.unwrap_err();
        assert!(format!("{e:#}").contains("--reconfigure"), "{e:#}");
        assert_eq!(count(&server.enrolls), 1);
        args.reconfigure = true;
        p.enroll(&args, Some("code-2")).await.unwrap();
        assert!(p.slot1().contains("aqn_cred2"));
        let slot2 = std::fs::read_to_string(p.layout.slot_enrollment(2)).unwrap();
        assert!(slot2.contains("aqn_slot2"));
    }

    /// Review #5: a failed key claim leaves slot 1 as it was and keeps the redeemed credential;
    /// the rerun resumes at the claim without a new code.
    #[tokio::test]
    async fn failed_claim_keeps_slot_one_and_resumes_without_a_code() {
        let server = fake_server(1).await;
        let p = profile();
        let old = EnrollmentFile {
            server_url: server.origin.clone(),
            node_id: Uuid::from_u128(7),
            credential: "aqn_old".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        old.save(&p.layout.slot_enrollment(1)).unwrap();
        let mut args = p.args(&server.origin);
        args.reconfigure = true;
        let e = p.enroll(&args, Some("code-1")).await.unwrap_err();
        assert!(format!("{e:#}").contains("is in use"), "{e:#}");
        assert!(p.slot1().contains("aqn_old"), "slot 1 is untouched");
        assert!(pending_enrollment(&p.layout).exists() && !p.layout.service_toml().exists());
        args.reconfigure = false;
        p.enroll(&args, None).await.unwrap();
        assert_eq!((count(&server.enrolls), count(&server.claims)), (1, 2));
        assert!(p.slot1().contains("aqn_cred1"));
        assert!(!pending_enrollment(&p.layout).exists() && p.layout.service_toml().exists());
        let other = p.args("https://other.example.com");
        assert!(
            p.enroll(&other, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("--reconfigure")
        );
    }

    /// Round 2, D: the claim succeeded but the run died before the slot files were written. The
    /// server returns slot 2's credential only once, so the rerun names the precise remedy; and
    /// a run that died after the files were installed (pending not yet removed) completes.
    #[tokio::test]
    async fn resume_after_a_successful_claim_is_done_or_names_the_remedy() {
        let server = fake_server(0).await;
        let p = profile();
        let first = p.pending(&server.origin);
        let request = SlotsRequest {
            schema_version: 1,
            machine_key: "desk".into(),
            slots: 2,
        };
        let lost = ServerLink::from_enrollment(&first)
            .unwrap()
            .slots(&request)
            .await
            .unwrap();
        assert!(
            lost.slots[0].credential.is_some(),
            "minted, then the run died"
        );
        let e = format!(
            "{:#}",
            p.enroll(&p.args(&server.origin), None).await.unwrap_err()
        );
        assert!(
            e.contains("Revoke the machine") && e.contains("--reconfigure"),
            "{e}"
        );
        assert!(!e.contains("rerun"), "{e}");
        assert_eq!(p.slot1(), "", "nothing installed");
        // The earlier run had staged (or installed) every slot file before it died.
        for (slot, node, credential) in [(1, 1, "aqn_cred1"), (2, 99, "aqn_slot2")] {
            let file = EnrollmentFile {
                node_id: Uuid::from_u128(node),
                credential: credential.into(),
                ..first.clone()
            };
            file.save(&staged(&p.layout.slot_enrollment(slot))).unwrap();
        }
        p.enroll(&p.args(&server.origin), None).await.unwrap();
        let slot2 = std::fs::read_to_string(p.layout.slot_enrollment(2)).unwrap();
        assert!(slot2.contains("aqn_slot2"));
        assert!(!staged(&p.layout.slot_enrollment(2)).exists());
        assert!(!pending_enrollment(&p.layout).exists() && p.layout.service_toml().exists());
        assert_eq!(count(&server.enrolls), 0, "no code was redeemed");
    }

    /// Round 2, E: an Access refusal at a resumed claim says so, and --reconfigure on the
    /// resume replaces the stored Access token without a new code.
    #[tokio::test]
    async fn resume_refused_by_access_takes_new_access_credentials() {
        let server = fake_server_with(0, Some("new-id")).await;
        let p = profile();
        p.pending(&server.origin);
        let mut args = p.args(&server.origin);
        let e = format!("{:#}", p.enroll(&args, None).await.unwrap_err());
        assert!(
            e.contains("Cloudflare Access refused the stored token"),
            "{e}"
        );
        assert!(e.contains("--reconfigure"), "{e}");
        (args.reconfigure, args.cf_service_token) = (true, true);
        p.enroll_access(&args, "new-id").await.unwrap();
        assert!(p.slot1().contains("new-id") && p.slot1().contains("aqn_cred1"));
        assert_eq!(count(&server.enrolls), 0);
    }

    /// D8: the machine label is the machine key unless --name gives one.
    #[tokio::test]
    async fn machine_label_defaults_to_the_machine_key() {
        let server = fake_server(0).await;
        let p = profile();
        let mut args = p.args(&server.origin);
        args.name = None;
        p.enroll(&args, Some("code-1")).await.unwrap();
        let machine: serde_json::Value =
            serde_json::from_slice(&std::fs::read(p.layout.machine_json()).unwrap()).unwrap();
        assert_eq!(
            (machine["key"].as_str(), machine["label"].as_str()),
            (Some("desk"), Some("desk"))
        );
        args.reconfigure = true;
        args.name = Some("Desk PC".into());
        p.enroll(&args, Some("code-2")).await.unwrap();
        let machine: serde_json::Value =
            serde_json::from_slice(&std::fs::read(p.layout.machine_json()).unwrap()).unwrap();
        assert_eq!(machine["label"], "Desk PC");
    }

    /// D9: the CLI uses service.toml's state and runtime roots, so purge removes a custom
    /// state root (and `service measure` takes the service's measurement lock).
    #[tokio::test]
    async fn purge_and_measure_use_the_configured_state_root() {
        let server = fake_server(0).await;
        let p = profile();
        p.enroll(&p.args(&server.origin), Some("code-1"))
            .await
            .unwrap();
        let custom = p.dir.path().join("aq-state");
        let mut config = ServiceConfig::load(&p.layout.service_toml()).unwrap();
        config.state_root = custom.clone();
        config.runtime_root = Some(p.dir.path().join("run/local"));
        config.save(&p.layout.service_toml()).unwrap();
        std::fs::create_dir_all(custom.join("slots/1/current")).unwrap();
        let defaults = Layout {
            config_dir: p.layout.config_dir.clone(),
            state_root: p.dir.path().join("default-state"),
            runtime_root: p.dir.path().join("default-run"),
        };
        let (layout, _) = loaded_in(defaults, "local").unwrap();
        assert_eq!(layout.state_root, custom);
        assert_eq!(layout.measure_lock(), custom.join("measure.lock"));
        assert_eq!(layout.runtime_root, p.dir.path().join("run/local"));
        layout.prepare_runtime().unwrap();
        purge_dirs(&layout).unwrap();
        assert!(!custom.exists() && !layout.runtime_root.exists());
        assert!(!p.layout.config_dir.exists());
    }

    /// D2 under the proof-format ruling, in the service's terms: a lost guardian is held,
    /// never reported stopped. The reason names the outside proof found from real facts: a
    /// changed boot id, or the unit teardown attested from a cgroup holding only this process
    /// (a fake cgroup file); with a foreign pid there is no teardown proof.
    #[test]
    fn a_lost_guardian_is_held_and_names_its_outside_proof() {
        use aq_node_service::invocation::{holds_only, own_cgroup_procs, unit_of};
        use aq_session_host::lost_guardian::{PriorGone, recover_stopped_with};
        const OLD: &str = "00000000-0000-4000-8000-00000000000a";
        const NOW: &str = "00000000-0000-4000-8000-00000000000b";
        let id = Uuid::from_u128(1);
        let process = |pid, start_ticks| aq_session_host::ProcessEvidence { pid, start_ticks };
        let running = |boot: &str| aq_session_host::HostObservation {
            guardian: aq_session_host::GuardianIdentity {
                intent: aq_session_host::LaunchIntent {
                    launch_id: id,
                    session: aq_session_protocol::SessionIdentity::Remote {
                        node_id: id,
                        boot_id: id,
                        attempt_id: id,
                        assignment_generation: 1,
                        session_id: id,
                    },
                    manifest_digest: aq_session_protocol::Digest::of(b"m"),
                    configuration_digest: aq_session_protocol::Digest::of(b"c"),
                },
                nonce: id,
                process: process(4_000_000, 500),
                machine_boot_id: boot.into(),
                pid_namespace: "pid:[4026531836]".into(),
            },
            phase: aq_session_host::HostPhase::Running,
            pane: None,
            permit_sequence: 3,
            permit_deadline_boot_ms: 90_000,
            observed_boot_ms: 60_000,
            reaped_count: 0,
            kernel_wait_echild: false,
            root_status: None,
            stop_reason: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("runtime-wiped");
        let held = |last: &aq_session_host::HostObservation, prior: Option<&PriorGone>| {
            let result = recover_stopped_with(&gone, &last.guardian.intent, Some(last), NOW, prior);
            let (proof, why) = unproven(&result).unwrap();
            assert_eq!(
                proof.is_some(),
                !why.contains("host stop unproven"),
                "{why}"
            );
            why
        };
        let unit_dir = dir.path().join("app.slice/aq-node@t.service");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let procs = own_cgroup_procs("0::/app.slice/aq-node@t.service\n", dir.path()).unwrap();
        let own = std::process::id();
        let attested = |listed: &str| {
            std::fs::write(&procs, listed).unwrap();
            let read = std::fs::read_to_string(&procs).unwrap();
            let gone = holds_only(&read, own, |pid| (pid == own + 1).then_some(own));
            gone.then(|| PriorGone {
                boot: NOW.into(),
                start_ticks: 900,
                invocation_id: "0123456789abcdef0123456789abcdef".into(),
                unit: unit_of(&procs).unwrap(),
            })
        };
        let rebooted = held(&running(OLD), None);
        assert!(
            rebooted.contains("\"proof_kind\":\"boot_changed\""),
            "{rebooted}"
        );
        let ours = attested(&format!("{own}\n{}\n", own + 1));
        let torn = held(&running(NOW), ours.as_ref());
        assert!(
            torn.contains("\"proof_kind\":\"cgroup_teardown\""),
            "{torn}"
        );
        assert!(torn.contains("aq-node@t.service"), "{torn}");
        let foreign = attested(&format!("{own}\n1\n"));
        assert!(foreign.is_none(), "a foreign pid: no attestation");
        let none = held(&running(NOW), foreign.as_ref());
        assert!(none.contains("host stop unproven"), "{none}");
        assert!(unproven(&Ok::<_, anyhow::Error>(())).is_none());
    }

    /// D2 ruling C: with an outside proof, the lost attempt's sandbox is stopped (kept), the
    /// release is posted with the proof, the record says nothing was salvaged, and the slot
    /// is freed. A refused release or a sandbox that cannot be stopped holds the slot.
    #[tokio::test]
    async fn a_proven_lost_attempt_is_released_and_frees_the_slot() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        let dir = tempfile::tempdir().unwrap();
        let attempt = Uuid::from_u128(77);
        let proof = aq_protocol::StopProof::CgroupTeardown {
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: "aq-node@t.service".into(),
        };
        let answer = |r: &aq_protocol::LostAttemptRelease| aq_protocol::LostAttemptReleased {
            schema_version: 1,
            attempt_id: r.attempt_id,
            reason: aq_protocol::LostReason::of(&r.proof).unwrap(),
            charged: true,
            released: true,
            held: None,
        };
        let stopped = AtomicBool::new(false);
        let posted = std::sync::Mutex::new(None);
        let outcome = release_lost(
            async || {
                stopped.store(true, SeqCst);
                Ok(())
            },
            async |r| {
                anyhow::ensure!(stopped.load(SeqCst), "stopped before the post");
                *posted.lock().unwrap() = Some(r.clone());
                Ok(answer(&r))
            },
            dir.path(),
            attempt,
            proof.clone(),
            "lost",
        )
        .await;
        assert_eq!(outcome, RecoverOutcome::Lost);
        let sent = posted.lock().unwrap().clone().unwrap();
        assert_eq!((sent.attempt_id, &sent.proof), (attempt, &proof));
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join(format!("lost-{attempt}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(record["salvaged"], false);
        assert_eq!(record["reason"], "lost_after_crash");
        assert!(
            record["sandbox"]
                .as_str()
                .unwrap()
                .contains("release-sandbox")
        );
        let refused = release_lost(
            async || Ok(()),
            async |_| anyhow::bail!("409 lost_attempt_effects_present"),
            dir.path(),
            Uuid::from_u128(78),
            proof.clone(),
            "lost",
        )
        .await;
        assert!(matches!(refused, RecoverOutcome::Held(why) if why.contains("refused")));
        let unstoppable = release_lost(
            async || anyhow::bail!("docker down"),
            async |_| panic!("never posted while the sandbox may run"),
            dir.path(),
            Uuid::from_u128(79),
            proof,
            "lost",
        )
        .await;
        assert!(
            matches!(unstoppable, RecoverOutcome::Held(why) if why.contains("could not be stopped"))
        );
    }

    /// The lost attempt's running sandbox is stopped, not destroyed; others are untouched.
    #[tokio::test]
    async fn a_lost_attempts_sandbox_is_stopped_and_kept() {
        use aq_worker::backend::{
            CreateRequest, FakeBackend, Ownership, PlacementBackend, PrivateMaterial, SandboxState,
            TemplateDigest,
        };
        let mut backend = FakeBackend::default();
        let node = Uuid::from_u128(1);
        let template = TemplateDigest::parse(format!("sha256:{}", "1".repeat(64))).unwrap();
        let (lost, other) = (Uuid::new_v4(), Uuid::new_v4());
        for attempt_id in [lost, other] {
            let owner = Ownership {
                node_id: node,
                boot_id: Uuid::new_v4(),
                attempt_id,
            };
            let request = CreateRequest::new(owner, template.clone(), 1 << 40, vec![]);
            let sandbox = backend.create(&request).await.unwrap();
            let material = PrivateMaterial::new(b"fixture".to_vec());
            backend
                .deliver_private_material(&sandbox, material)
                .await
                .unwrap();
            backend.start_runner(&sandbox).await.unwrap();
        }
        stop_attempt_sandboxes(&mut backend, node, lost)
            .await
            .unwrap();
        let states: std::collections::BTreeMap<Uuid, SandboxState> =
            (backend.list_owned(node).await.unwrap())
                .into_iter()
                .map(|e| (e.sandbox.owner.attempt_id, e.state))
                .collect();
        assert_eq!(
            states[&lost],
            SandboxState::Stopped,
            "kept for the operator"
        );
        assert_eq!(states[&other], SandboxState::Running);
    }
}
