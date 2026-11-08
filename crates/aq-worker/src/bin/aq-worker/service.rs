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
}

pub async fn dispatch(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Enroll(args) => enroll(args).await,
        ServiceCmd::Install { profile } => install(&profile),
        ServiceCmd::Uninstall { profile, purge } => uninstall(&profile, purge),
        ServiceCmd::Status { profile } => status(&profile),
        ServiceCmd::Measure { profile } => measure(&profile).await,
    }
}

/// The native pipeline behind the supervisor: one `run_until_attempt` per call.
pub struct NativeSlotBackend {
    template: PathBuf,
}
impl NativeSlotBackend {
    pub fn new(template: PathBuf) -> Self {
        Self { template }
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
            if let Err(e) = link.handoff(&notice).await {
                tracing::warn!(%attempt, "handoff notice: {e:#}");
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
    async fn measure(&self, _token_file: &Path, _model: &str) -> Result<MeasurementSubmission> {
        bail!("measurement not built")
    }
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|r| r.to_lowercase().contains("microsoft"))
}

pub async fn enroll(args: ServiceEnrollArgs) -> Result<()> {
    let hostname = crate::hostname();
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
    let layout = Layout::for_profile(&args.profile)?;
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
    let aq_git = AqGitConfig {
        binary: std::fs::canonicalize(&args.aq_git)
            .with_context(|| format!("aq-git {}", args.aq_git.display()))?,
        git_executable: args.git_executable.clone(),
    };
    let secrets = crate::read_secrets(args.cf_service_token, false)?;
    let (cf_id, cf_secret) = secrets.cf.map_or((None, None), |(i, s)| (Some(i), Some(s)));

    let client = Client::new(&args.server, None, cf_id.clone(), cf_secret.clone())?;
    let enrolled = client
        .enroll(&EnrollRequest {
            code: secrets.code,
            name: name.clone(),
            worker_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;
    let slot_file = |node_id: Uuid, credential: String| EnrollmentFile {
        server_url: args.server.clone(),
        node_id,
        credential,
        cf_client_id: cf_id.clone(),
        cf_client_secret: cf_secret.clone(),
    };
    let first = slot_file(enrolled.node_id, enrolled.credential);
    first.save(&layout.slot_enrollment(1))?;
    // Claim the machine key and mint the siblings, always, even with one slot.
    let minted = ServerLink::from_enrollment(&first)?
        .slots(&SlotsRequest { schema_version: 1, machine_key: machine_key.clone(), slots: args.slots })
        .await
        .map_err(|e| match e.downcast_ref::<LinkError>() {
            Some(l) if l.status == 409 => anyhow::anyhow!("machine key {machine_key} is in use by another machine; revoke it from the Swarm page or pass --machine-key"),
            _ => e,
        })?;
    let mut ids = vec![first.node_id];
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
        slot_file(entry.node_id, credential.clone()).save(&layout.slot_enrollment(slot))?;
        ids.push(entry.node_id);
    }
    write_machine_json(&layout.machine_json(), &machine_key, &name, is_wsl())?;
    let native = layout.config_dir.join("native.json");
    write_private(&native, &template)?;
    ServiceConfig {
        schema_version: 1,
        profile: args.profile.clone(),
        machine_key: machine_key.clone(),
        slots: args.slots,
        native_template: std::fs::canonicalize(&native)?,
        state_root: layout.state_root.clone(),
        aq_git,
        repositories,
        measurement: MeasurementConfig {
            enabled: args.measurement_token_file.is_some(),
            token_file: args.measurement_token_file.clone(),
            model: args.measurement_model.clone(),
            ..MeasurementConfig::default()
        },
        shutdown_grace_ms: 120_000,
    }
    .save(&layout.service_toml())?;
    println!(
        "machine {machine_key}: slots {}",
        ids.iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );
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
    let (layout, _) = loaded(profile)?;
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
async fn measure(profile: &str) -> Result<()> {
    let (layout, config) = loaded(profile)?;
    let m = &config.measurement;
    let (Some(file), Some(model)) = (&m.token_file, &m.model) else {
        bail!("measurement is not configured for profile {profile}");
    };
    let link = ServerLink::from_enrollment(&EnrollmentFile::load(&layout.slot_enrollment(1))?)?;
    let submission = NativeSlotBackend::new(config.native_template.clone())
        .measure(file, model)
        .await?;
    let pin = link.submit_measurement(&submission).await?;
    println!("qualification accepted until {} ms", pin.expires_at_ms);
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
    let link = Arc::new(ServerLink::from_enrollment(&EnrollmentFile::load(
        &layout.slot_enrollment(1),
    )?)?);
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
    let backend = Arc::new(NativeSlotBackend::new(config.native_template.clone()));
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
}
