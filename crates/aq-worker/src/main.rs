#[cfg(feature = "test-support")]
mod fixture_responses_cli;
#[cfg(feature = "test-support")]
mod native_fixture;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use aq_protocol::{EnrollRequest, PROTOCOL_VERSION};
use aq_worker::client::{CallError, Client, run_loop};
use aq_worker::config::{WorkerConfig, default_path};
use clap::{Parser, Subcommand};

/// Agent Q worker connector.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to worker.toml (default: $XDG_CONFIG_HOME/agent-q/worker.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or scan sandbox templates on an explicitly configured build node.
    Template {
        #[command(subcommand)]
        command: TemplateCmd,
    },
    /// Enroll this machine. Secrets are read from a prompt or from stdin lines
    /// (code, then CF client id and secret with --cf-service-token).
    Enroll {
        #[arg(long)]
        server: String,
        /// Worker name (default: the hostname).
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cf_service_token: bool,
    },
    /// Own node identity, capacity and sandbox lifecycle until stopped.
    Run {
        /// Operator JSON configuration; B2 transport must be installed for admission.
        #[arg(long)]
        controller_config: PathBuf,
    },
    /// Run the protocol-2 synthetic execution candidate (no real provider harness).
    RunExecution {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        fixture_agent: PathBuf,
        #[arg(long, default_value_t = 1)]
        max_agents: u32,
        #[arg(long, default_value_t = 60)]
        idle_timeout_secs: u64,
        #[arg(long)]
        exit_after_result: bool,
        /// Configure the local source adapter with an immutable aq-git executable pin.
        #[arg(long, requires_all=["aq_git_sha256","aq_git_state_dir"])]
        aq_git_binary: Option<PathBuf>,
        #[arg(long, requires = "aq_git_binary")]
        aq_git_sha256: Option<String>,
        #[arg(long, requires = "aq_git_binary")]
        aq_git_state_dir: Option<PathBuf>,
        /// Trusted Git executable used by aq-git.
        #[arg(long, default_value = "/usr/bin/git")]
        git_executable: PathBuf,
        #[arg(long,default_value_t=64*1024*1024)]
        source_max_bundle_bytes: u64,
        #[arg(long,default_value_t=512*1024*1024)]
        source_max_expanded_bytes: u64,
        /// Enable resumable authenticated base downloads and source handoff.
        #[arg(long, requires = "aq_git_binary")]
        source_transfer: bool,
        /// Fault injection for the deterministic integration fixture.
        #[arg(long)]
        drop_acceptance_response_once: bool,
        /// Fault injection for the deterministic integration fixture.
        #[arg(long)]
        drop_report_response_once: bool,
        /// Explicit deterministic local recovery test injection.
        #[arg(long)]
        fault_checkpoint: Option<String>,
        #[arg(long, default_value = "crash")]
        fault_action: String,
    },
    /// Execute one fresh native attempt using an installed qualified executor.
    /// Until a concrete production executor is installed, refuses before admission.
    RunNative {
        #[arg(long)]
        native_config: PathBuf,
        /// Explicit local qualification transport; requires a confined fixture and provider.
        #[cfg(feature = "test-support")]
        #[arg(long)]
        development_loopback: bool,
        #[cfg(feature = "test-support")]
        #[arg(long, requires = "development_loopback")]
        development_provider: Option<String>,
        #[cfg(feature = "test-support")]
        #[arg(long, requires = "development_loopback")]
        fixture_preflight_gate: bool,
        /// Once-only in-process fixture B1 ACK-loss reconciliation; preserves native failure.
        #[cfg(feature = "test-support")]
        #[arg(long, requires_all = ["development_loopback", "fixture_preflight_gate"])]
        fixture_replay_lost_b1_ack: bool,
    },
    /// Consume a frozen local handoff with a runner-private delegated permit.
    SandboxRun {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        /// Runner-private permit JSON delivered by the node controller.
        #[arg(long)]
        permit_file: PathBuf,
        /// Coordinator or runner-owned AQ proxy origin; no node enrollment file.
        #[arg(long)]
        server_url: String,
    },
    /// Validate native configuration and installed executor without admission.
    CheckNative {
        #[arg(long)]
        native_config: PathBuf,
    },
    /// Pure finite qualification vectors/request checks; never executes a model.
    #[cfg(feature = "test-support")]
    FixtureResponses,
    /// Print the node id and server (never the credential).
    Status,
}

#[derive(Subcommand)]
enum TemplateCmd {
    /// Build, finalize, scan and allowlist a pinned recipe (requires Python 3 and BuildKit).
    Build {
        recipe: PathBuf,
        #[arg(long)]
        build_config: PathBuf,
        /// New output directory for the OCI archive and provenance.
        #[arg(long)]
        output: PathBuf,
    },
    /// Scan every layer of a single-platform OCI image archive without admission.
    Scan { archive: PathBuf },
}

fn usage_err(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(2)
}

fn hostname() -> Option<String> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
}

type Reader = Box<dyn FnMut(&str) -> anyhow::Result<String>>;

struct Secrets {
    code: String,
    cf: Option<(String, String)>,
}

fn read_secrets(cf: bool) -> anyhow::Result<Secrets> {
    let mut next: Reader = if std::io::stdin().is_terminal() {
        Box::new(|prompt| Ok(rpassword::prompt_password(prompt)?))
    } else {
        let mut lines = std::io::stdin().lock().lines();
        Box::new(move |what| match lines.next() {
            Some(l) => Ok(l?),
            None => anyhow::bail!("stdin ended before the {what}"),
        })
    };
    let code = next("Enrollment code: ")?.trim().to_string();
    let cf = if cf {
        let id = next("Cloudflare Access client id: ")?.trim().to_string();
        let secret = next("Cloudflare Access client secret: ")?
            .trim()
            .to_string();
        Some((id, secret))
    } else {
        None
    };
    if code.is_empty() {
        anyhow::bail!("enrollment code is empty");
    }
    Ok(Secrets { code, cf })
}

fn exit_for(e: &CallError) -> ExitCode {
    match e {
        CallError::Revoked | CallError::InvalidCredential => ExitCode::from(3),
        CallError::Incompatible(_) => ExitCode::from(4),
        CallError::AccessRejected => ExitCode::from(5),
        _ => ExitCode::from(2),
    }
}

fn run_error(e: &anyhow::Error) -> (String, ExitCode) {
    (
        format!("{e:#}"),
        e.downcast_ref::<CallError>()
            .map(exit_for)
            .unwrap_or_else(|| ExitCode::from(2)),
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

fn configured_native_executor(
    worker: &WorkerConfig,
    config: &aq_worker::native::runner::NativeConfig,
) -> anyhow::Result<Option<aq_worker::native::runner::CodexExecutor>> {
    if config.executor.is_none() {
        return Ok(None);
    }
    Ok(Some(aq_worker::native::runner::CodexExecutor::configured(
        worker, config,
    )?))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let sandbox_agent = if matches!(&cli.command, Cmd::SandboxRun { .. }) {
        match aq_session_host::users::bootstrap_sandbox_users() {
            Ok(agent) => Some(agent),
            Err(error) => return usage_err(error),
        }
    } else {
        None
    };
    run(cli, sandbox_agent)
}
#[tokio::main]
async fn run(cli: Cli, sandbox_agent: Option<aq_session_host::users::AgentIdentity>) -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = cli.config.unwrap_or_else(default_path);
    match cli.command {
        Cmd::Template { command } => {
            let args = match command {
                TemplateCmd::Build {
                    recipe,
                    build_config,
                    output,
                } => vec![
                    "build".into(),
                    recipe.into_os_string(),
                    "--build-config".into(),
                    build_config.into_os_string(),
                    "--output".into(),
                    output.into_os_string(),
                ],
                TemplateCmd::Scan { archive } => vec!["scan".into(), archive.into_os_string()],
            };
            match aq_worker::template::run(&args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => {
                    usage_err("template operation refused; Python 3 and valid inputs required")
                }
            }
        }
        Cmd::RunExecution {
            state_dir,
            fixture_agent,
            max_agents,
            idle_timeout_secs,
            exit_after_result,
            drop_acceptance_response_once,
            drop_report_response_once,
            fault_checkpoint,
            fault_action,
            aq_git_binary,
            aq_git_sha256,
            aq_git_state_dir,
            git_executable,
            source_max_bundle_bytes,
            source_max_expanded_bytes,
            source_transfer,
        } => {
            let result = async {
                if let Some(name) = fault_checkpoint {
                    aq_worker::fault::configure(name, fault_action)?;
                }
                let cfg = WorkerConfig::load(&path)?;
                aq_worker::driver::run_execution(
                    &cfg,
                    &state_dir,
                    &fixture_agent,
                    aq_worker::driver::DriverOptions {
                        source: aq_git_binary.map(|binary| aq_worker::driver::SourceOptions {
                            binary,
                            binary_sha256: aq_git_sha256.unwrap(),
                            state_dir: aq_git_state_dir.unwrap(),
                            git_executable,
                            max_bundle_bytes: source_max_bundle_bytes,
                            max_expanded_bytes: source_max_expanded_bytes,
                            transfer: source_transfer,
                        }),
                        max_agents,
                        idle_timeout_secs,
                        exit_after_result,
                        drop_acceptance_response_once,
                        drop_report_response_once,
                    },
                )
                .await
            }
            .await;
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => usage_err(format!("{e:#}")),
            }
        }
        Cmd::SandboxRun {
            manifest,
            state_dir,
            permit_file,
            server_url,
        } => {
            let result = (|| -> anyhow::Result<()> {
                let _agent = sandbox_agent
                    .ok_or_else(|| anyhow::anyhow!("sandbox user bootstrap required"))?;
                let frozen = aq_worker::native::runner::sandbox::FrozenLaunch::load(&manifest)?;
                frozen.validate_local(&state_dir)?;
                let _http = aq_worker::native::runner::sandbox::auth::RunnerHttp::for_execution(
                    &server_url,
                    &permit_file,
                    &frozen,
                )?;
                anyhow::bail!(
                    "sandbox runner B2 receipt codec is unavailable; permit transport validated, no launch attempted"
                );
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => usage_err(e),
            }
        }
        Cmd::CheckNative { native_config } => {
            let result = (|| -> anyhow::Result<()> {
                let worker = WorkerConfig::load(&path)?;
                let config = aq_worker::native::runner::NativeConfig::load(&native_config)?;
                config.validate()?;
                let mut executor = configured_native_executor(&worker, &config)?
                    .ok_or_else(|| anyhow::anyhow!("native executor is not configured"))?;
                aq_worker::native::runner::NativeExecutor::preflight(&mut executor, &config)?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    println!("Native configuration and installed executor are valid");
                    ExitCode::SUCCESS
                }
                Err(_) => usage_err("native configuration check refused"),
            }
        }
        Cmd::RunNative {
            native_config,
            #[cfg(feature = "test-support")]
            development_loopback,
            #[cfg(feature = "test-support")]
            development_provider,
            #[cfg(feature = "test-support")]
            fixture_preflight_gate,
            #[cfg(feature = "test-support")]
            fixture_replay_lost_b1_ack,
        } => {
            let result = async {
                #[cfg(feature = "test-support")]
                let fixture = native_fixture::Options::from_flags(
                    development_loopback,
                    development_provider,
                    fixture_preflight_gate,
                    fixture_replay_lost_b1_ack,
                )?;
                #[cfg(feature = "test-support")]
                if fixture.is_some() {
                    native_fixture::verify_environment()?;
                }
                let worker = WorkerConfig::load(&path)?;
                let config = aq_worker::native::runner::NativeConfig::load(&native_config)
                    .inspect_err(|_| {
                        tracing::info!(stage = "native_config_load", "native execution refused");
                    })?;
                #[cfg(feature = "test-support")]
                let executor = match &fixture {
                    Some(options) => Some(options.executor(&worker)?),
                    None => configured_native_executor(&worker, &config)?,
                };
                #[cfg(not(feature = "test-support"))]
                let executor = configured_native_executor(&worker, &config)?;
                let (signal, shutdown) = tokio::sync::watch::channel(false);
                let watcher = tokio::spawn(async move {
                    shutdown_signal().await;
                    let _ = signal.send(true);
                });
                let result = if let Some(mut executor) = executor {
                    #[cfg(feature = "test-support")]
                    {
                        match fixture {
                            Some(options) => {
                                let mut executor = native_fixture::FixtureExecutor::new(
                                    executor,
                                    options.gate,
                                    options.replay_lost_b1_ack,
                                );
                                aq_worker::native::runner::run_once(
                                    &worker,
                                    config,
                                    &mut executor,
                                    shutdown,
                                )
                                .await
                            }
                            None => {
                                aq_worker::native::runner::run_once(
                                    &worker,
                                    config,
                                    &mut executor,
                                    shutdown,
                                )
                                .await
                            }
                        }
                    }
                    #[cfg(not(feature = "test-support"))]
                    {
                        aq_worker::native::runner::run_once(
                            &worker,
                            config,
                            &mut executor,
                            shutdown,
                        )
                        .await
                    }
                } else {
                    aq_worker::native::runner::run_once(
                        &worker,
                        config,
                        &mut aq_worker::native::runner::UnavailableExecutor,
                        shutdown,
                    )
                    .await
                };
                watcher.abort();
                result
            }
            .await;
            match result {
                Ok(disposition) => {
                    println!("native run: {disposition:?}; retained state preserved");
                    ExitCode::SUCCESS
                }
                Err(_) => usage_err(
                    "native execution unavailable or outcome unknown; preserve state for reconciliation",
                ),
            }
        }
        #[cfg(feature = "test-support")]
        Cmd::FixtureResponses => match fixture_responses_cli::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => usage_err("finite response fixture input refused"),
        },
        Cmd::Status => match WorkerConfig::load(&path) {
            Ok(c) => {
                println!("node id: {}", c.node_id);
                println!("server:  {}", c.server_url);
                ExitCode::SUCCESS
            }
            Err(e) => usage_err(format!("{e:#}")),
        },
        Cmd::Run { controller_config } => {
            let result: anyhow::Result<()> = async {
                use aq_worker::controller::{Controller, RunConfig, UnavailableProvider};
                use aq_worker::backend::docker_local::DockerLocalBackend;
                let worker = WorkerConfig::load(&path)?;
                let config = RunConfig::load(&controller_config)?;
                let backend = DockerLocalBackend::connect_unix(config.docker_local).await?;
                let mut controller = Controller::open(&config.state_dir, worker.node_id, config.node, backend, UnavailableProvider)?;
                let client = Client::from_config(&worker)?;
                let (tx, rx) = tokio::sync::watch::channel(false);
                let watcher = tokio::spawn(async move {
                    shutdown_signal().await;
                    let _ = tx.send(true);
                });
                let mut heartbeat_shutdown = rx.clone();
                tracing::info!(node_id = %controller.identity().node_id, boot_id = %controller.identity().boot_id, "node controller starting");
                let result = controller.run_with_heartbeat(rx, run_loop(&client, async move {
                        while !*heartbeat_shutdown.borrow() {
                            if heartbeat_shutdown.changed().await.is_err() { break; }
                        }
                    })).await;
                watcher.abort();
                result
            }.await;
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    let (message, code) = run_error(&e);
                    eprintln!("error: {message}");
                    code
                }
            }
        }
        Cmd::Enroll {
            server,
            name,
            cf_service_token,
        } => {
            let Some(name) = name.or_else(hostname) else {
                return usage_err("cannot determine hostname; pass --name");
            };
            let secrets = match read_secrets(cf_service_token) {
                Ok(s) => s,
                Err(e) => return usage_err(format!("{e:#}")),
            };
            let (cf_id, cf_secret) = match secrets.cf {
                Some((i, s)) => (Some(i), Some(s)),
                None => (None, None),
            };
            let client = match Client::new(&server, None, cf_id.clone(), cf_secret.clone()) {
                Ok(c) => c,
                Err(e) => return usage_err(format!("{e:#}")),
            };
            let resp = client
                .enroll(&EnrollRequest {
                    code: secrets.code,
                    name,
                    worker_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: PROTOCOL_VERSION,
                })
                .await;
            match resp {
                Ok(r) => {
                    let cfg = WorkerConfig {
                        server_url: server,
                        node_id: r.node_id,
                        credential: r.credential,
                        cf_client_id: cf_id,
                        cf_client_secret: cf_secret,
                    };
                    match cfg.save(&path) {
                        Ok(()) => {
                            println!("enrolled as node {}", cfg.node_id);
                            println!("config saved to {}", path.display());
                            ExitCode::SUCCESS
                        }
                        Err(e) => usage_err(format!("{e:#}")),
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_for(&e)
                }
            }
        }
    }
}

#[cfg(all(test, not(feature = "test-support")))]
#[test]
fn production_cli_does_not_expose_fixture_transport_flags() {
    assert!(
        Cli::try_parse_from([
            "aq-worker",
            "run-native",
            "--native-config",
            "/nonexistent",
            "--development-loopback",
            "--development-provider",
            "http://127.0.0.1:4321/",
        ])
        .is_err()
    );
}

#[cfg(all(test, not(feature = "test-support")))]
#[test]
fn production_cli_does_not_expose_lost_b1_ack_replay() {
    assert!(
        Cli::try_parse_from([
            "aq-worker",
            "run-native",
            "--native-config",
            "/nonexistent",
            "--fixture-replay-lost-b1-ack"
        ])
        .is_err()
    );
}

#[cfg(test)]
#[test]
fn run_requires_explicit_controller_configuration() {
    assert!(Cli::try_parse_from(["aq-worker", "run"]).is_err());
    assert!(Cli::try_parse_from(["aq-worker", "run", "--controller-config", "/node.json"]).is_ok());
}

#[cfg(test)]
#[test]
fn run_errors_preserve_cause_and_connector_exit_codes() {
    use aq_protocol::CompatibilityError;
    for (error, expected) in [
        (CallError::Revoked, 3),
        (CallError::InvalidCredential, 3),
        (
            CallError::Incompatible(CompatibilityError {
                error: "unsupported_protocol".into(),
                message: "protocol mismatch".into(),
                supported_min: 2,
                supported_max: 2,
            }),
            4,
        ),
        (CallError::AccessRejected, 5),
        (CallError::Other("probe nonce mismatch".into()), 2),
    ] {
        let cause = error.to_string();
        let error = anyhow::Error::new(error).context("foundation connector");
        let (message, code) = run_error(&error);
        assert_eq!(code, ExitCode::from(expected));
        assert!(message.contains(&cause));
        assert!(message.contains("foundation connector"));
    }
    for cause in [
        "controller root already owned",
        "journal checksum mismatch",
        "Docker socket unavailable",
    ] {
        let error = anyhow::anyhow!("{cause}").context("controller startup");
        let (message, code) = run_error(&error);
        assert_eq!(code, ExitCode::from(2));
        assert_eq!(message, format!("controller startup: {cause}"));
    }
}
