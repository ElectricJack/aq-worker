//! `aq-worker run --service-config` and `aq-worker service ...` (spec 0007 phase 1):
//! the glue between `aq-node-service` and the native pipeline.
use anyhow::{Context, Result, bail, ensure};
use aq_node_service::{
    AqGitConfig, AqGitFactory, AttemptOutcome, EnrollmentFile, Layout, LinkError,
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
        retained_attempts, run_until_attempt, salvage_interrupted,
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
        let mut cfg = NativeConfig::load(&self.template)?;
        cfg.state_dir = ctx.state_dir.clone();
        cfg.git_socket = ctx.state_dir.join("git.sock");
        cfg.git_root = ctx.state_dir.join("git");
        if let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
            &mut cfg.executor
        {
            settings.host.root = ctx.state_dir.join("host");
            if let Some(pin) = &ctx.renderer {
                settings.host.renderer = to_artifact_ref(pin)?;
            }
        } else {
            bail!(
                "the worker service needs the Claude docker-local executor in its native template"
            );
        }
        if let Some(pin) = &ctx.renderer {
            cfg.renderer = to_artifact_ref(pin)?;
        }
        cfg.validate()?;
        Ok(cfg)
    }
}
/// Converts a wire pin to the pipeline's artifact reference (same JSON shape).
fn to_artifact_ref<T: serde::de::DeserializeOwned>(pin: &ArtifactPin) -> Result<T> {
    serde_json::from_value(serde_json::to_value(pin)?).context("invalid renderer pin")
}
/// What a restarted node tells the server about an interrupted attempt: the reason the service
/// recorded before the cut (a deliberate stop stays exempt from budgets), else a plain restart.
fn restart_reason(state_dir: &Path, attempt: Uuid) -> HandoffReason {
    recorded_handoff(state_dir, attempt).unwrap_or(HandoffReason::Restart)
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
                reason: restart_reason(&ctx.state_dir, attempt),
            };
            if !self.notified.lock().unwrap().contains(&attempt) {
                match link.handoff(&notice).await {
                    Ok(()) => drop(self.notified.lock().unwrap().insert(attempt)),
                    Err(e) => tracing::warn!(%attempt, "handoff notice: {e:#}"),
                }
            }
            match salvage_interrupted(&worker, &cfg, attempt).await? {
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
    let secrets = move || {
        crate::read_secrets(cf, false).map(|s| Enrollment {
            code: s.code,
            cf: s.cf,
        })
    };
    enroll_in(&args, &layout, crate::hostname(), is_wsl(), secrets).await
}
/// What the operator types (or pipes) at enrollment.
struct Enrollment {
    code: String,
    cf: Option<(String, String)>,
}
/// Slot 1's credential after the code is redeemed and before the machine key claim succeeds.
/// A rerun resumes the claim from it instead of redeeming a new code.
fn pending_enrollment(layout: &Layout) -> PathBuf {
    layout.config_dir.join("enroll-pending.toml")
}

/// Enrollment, in this order: every input is checked before a secret is read or the server
/// is called; an enrolled profile is refused without `--reconfigure`; an unfinished one resumes
/// at the claim; slot 1's file is written only once the machine key claim succeeds.
async fn enroll_in(
    args: &ServiceEnrollArgs,
    layout: &Layout,
    hostname: Option<String>,
    wsl: bool,
    secrets: impl FnOnce() -> Result<Enrollment>,
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
    // Repository count and format, absolute paths (the token file too) and bounds.
    config.validate()?;

    let pending = pending_enrollment(layout);
    let first = if pending.exists() {
        let first = EnrollmentFile::load(&pending)?;
        ensure!(
            first.server_url == args.server,
            "an unfinished enrollment for {} is in {}; pass that --server, or delete the file to start over with a new code",
            first.server_url,
            pending.display()
        );
        println!("resuming the unfinished enrollment at the machine key claim; no code is needed");
        first
    } else {
        if layout.service_toml().exists() && !args.reconfigure {
            bail!(
                "profile {} is already enrolled; nothing was changed and no code was used. To enroll it again (for example after a revoke), pass --reconfigure",
                args.profile
            );
        }
        let typed = secrets()?;
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
            _ => e.context("the machine key claim failed; rerun this command to resume it (the code is not needed again)"),
        })?;
    let mut files = vec![first.clone()];
    for slot in 2..=args.slots {
        let entry = minted
            .slots
            .iter()
            .find(|s| s.slot == slot)
            .with_context(|| format!("the server did not return slot {slot}"))?;
        let Some(credential) = &entry.credential else {
            bail!(
                "slot {slot} exists on the server but its credential is not on this machine; revoke the machine from the Swarm page and enroll again"
            );
        };
        files.push(EnrollmentFile {
            node_id: entry.node_id,
            credential: credential.clone(),
            ..first.clone()
        });
    }
    for (slot, file) in (1..).zip(&files) {
        file.save(&layout.slot_enrollment(slot))?;
    }
    write_machine_json(&layout.machine_json(), &machine_key, &name, wsl)?;
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

fn loaded(profile: &str) -> Result<(Layout, ServiceConfig)> {
    let layout = Layout::for_profile(profile)?;
    let config = ServiceConfig::load(&layout.service_toml()).with_context(|| {
        format!("profile {profile} is not enrolled; run `aq-worker service enroll` first")
    })?;
    Ok((layout, config))
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
    let layout = Layout::for_profile(profile)?;
    let manager = aq_node_service::detect()?;
    if manager.status(profile)? == ServiceState::NotInstalled {
        println!("aq-node@{profile}: not installed");
    } else {
        manager.uninstall(profile)?;
        println!("aq-node@{profile}: removed");
    }
    if purge {
        for dir in [&layout.config_dir, &layout.state_root] {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("remove {}", dir.display())),
            }
        }
        println!("configuration and state purged");
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

/// `aq-worker run --service-config`: supervise every slot until SIGTERM or ctrl-c.
pub async fn run_service(service_config: &Path) -> Result<()> {
    let config = ServiceConfig::load(service_config)?;
    let config_dir = service_config
        .parent()
        .context("service config has no directory")?;
    let layout = Layout {
        config_dir: config_dir.to_path_buf(),
        state_root: config.state_root.clone(),
    };
    let slot1 = EnrollmentFile::load(&layout.slot_enrollment(1))?;
    let link = Arc::new(ServerLink::from_enrollment(&slot1)?);
    // A measurement cut short (kill, crash, reboot) left its sandbox: release it before work.
    match crate::measure::sweep(&config.native_template, slot1.node_id).await {
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
    tracing::info!(profile = %config.profile, machine = %config.machine_key, slots = config.slots, "worker service starting");
    let backend = Arc::new(NativeSlotBackend::new(
        config.native_template.clone(),
        slot1.node_id,
    ));
    let git = Arc::new(AqGitFactory(config.aq_git.clone()));
    let result = Supervisor::new(config, layout, backend, link, git)
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

    #[test]
    fn restart_reason_follows_the_handoff_marker() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Uuid::from_u128(9);
        let driver = dir.path().join("host/driver-1");
        std::fs::create_dir_all(&driver).unwrap();
        std::fs::write(
            driver.join("driver.json"),
            serde_json::json!({"owner": {"attempt_id": attempt}}).to_string(),
        )
        .unwrap();
        assert_eq!(restart_reason(dir.path(), attempt), HandoffReason::Restart);
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
            assert_eq!(restart_reason(dir.path(), attempt), expected);
        }
        assert_eq!(
            restart_reason(dir.path(), Uuid::from_u128(10)),
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
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        let (enrolls, claims) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (e, c) = (enrolls.clone(), claims.clone());
        let app = Router::new()
            .route("/v1/worker/enroll", post(move || async move {
                let n = e.fetch_add(1, SeqCst) + 1;
                Json(serde_json::json!({"node_id": Uuid::from_u128(n.into()),
                    "credential": format!("aqn_cred{n}"), "heartbeat_interval_secs": 30}))
            }))
            .route("/v1/worker/slots", post(move || async move {
                if c.fetch_add(1, SeqCst) < refuse_claims {
                    let body = serde_json::json!({"error": "conflict", "message": "in use"});
                    return (StatusCode::CONFLICT, Json(body));
                }
                let body = serde_json::json!({"schema_version": 1, "machine_key": "desk", "slots": [
                    {"slot": 2, "node_id": Uuid::from_u128(99), "credential": "aqn_slot2"}]});
                (StatusCode::OK, Json(body))
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
            let secrets = move || {
                let code = code.expect("no secret may be read here");
                Ok(Enrollment { code, cf: None })
            };
            enroll_in(args, &self.layout, Some("desk".into()), false, secrets).await
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
}
