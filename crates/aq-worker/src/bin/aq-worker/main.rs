#[cfg(feature = "test-support")]
mod fixture_responses_cli;
mod host_terminal;
mod manual;
mod manual_stream;
mod native_commands;
mod native_diagnostics;
#[cfg(feature = "test-support")]
mod native_fixture;
mod service;
mod service_commands;
mod worker_commands;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use aq_protocol::{EnrollRequest, PROTOCOL_VERSION};
use aq_worker::client::{CallError, Client, run_loop};
use aq_worker::config::{RunnerAccess, WorkerConfig, default_path, runner_access_path};
use clap::{Parser, Subcommand};

/// Agent Q worker connector.
#[derive(Parser)]
#[command(version = env!("AQ_WORKER_VERSION"))]
struct Cli {
    /// Path to worker.toml (default: $XDG_CONFIG_HOME/agent-q/worker.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve owner host shells without restarting the agent worker.
    HostTerminalRelay {
        #[arg(long)]
        service_config: PathBuf,
    },
    /// Relay interactive terminal bytes without owning or restarting agent processes.
    TerminalRelay {
        #[arg(long)]
        service_config: PathBuf,
    },
    /// Build or scan sandbox templates on an explicitly configured build node.
    Template {
        #[command(subcommand)]
        command: TemplateCmd,
    },
    /// Enroll this machine. Secrets are read from a prompt or from stdin lines
    /// (code, then CF client id and secret with --cf-service-token, then the
    /// runner Access client id and secret with --runner-access-token).
    Enroll(EnrollArgs),
    /// Own node identity, capacity and sandbox lifecycle until stopped.
    Run {
        /// Legacy B5b controller configuration (admission stays fenced).
        #[arg(
            long,
            conflicts_with = "service_config",
            required_unless_present = "service_config"
        )]
        controller_config: Option<PathBuf>,
        /// Spec 0007 service: multi-slot docker-local work from service.toml.
        #[arg(long)]
        service_config: Option<PathBuf>,
    },
    /// Install, enroll and inspect the worker service (spec 0007 phase 1).
    Service {
        #[command(subcommand)]
        command: service::ServiceCmd,
    },
    /// Claim and run server check attempts (aq.check.sandbox/v1) on the
    /// Docker-local sandbox, reporting exactly one stop each.
    RunChecks {
        /// Operator JSON: journal_dir, checkpoint_dir, docker_local, runner.
        #[arg(long)]
        checks_config: PathBuf,
    },
    /// Import evidence-backed historical Boat VM lifecycle facts, then replay.
    /// Stop the controller first: this uses its exclusive Boat journal lock.
    BoatVmTimeBackfill {
        #[arg(long)]
        controller_config: PathBuf,
        /// JSON array of H2 VM-time facts; no credentials or no-create guesses.
        #[arg(long)]
        facts: PathBuf,
    },
    /// Run the protocol-2 synthetic execution candidate (no real provider harness).
    #[cfg(feature = "synthetic-runtime")]
    RunExecution(RunExecutionArgs),
    /// Execute one fresh native attempt using an installed qualified executor.
    /// Until a concrete production executor is installed, refuses before admission.
    RunNative(RunNativeArgs),
    /// Consume a frozen local handoff with a runner-private delegated permit.
    SandboxRun(SandboxRunArgs),
    /// MS1: print the public Claude docker-local Definition bodies (JSON).
    #[command(hide = true)]
    Ms1Definitions,
    /// MS1 pane program: drive one Claude attempt in a docker-local sandbox.
    /// Started only by the native session host; no node credential is read.
    #[command(hide = true)]
    SandboxHarness {
        #[arg(long)]
        driver_config: PathBuf,
    },
    /// Validate native configuration and installed executor without admission.
    CheckNative {
        #[arg(long)]
        native_config: PathBuf,
    },
    /// Operator release: destroy the preserved (stopped) docker-local sandbox
    /// of one attempt, recording the reason as destroy evidence.
    ReleaseNativeSandbox {
        #[arg(long)]
        native_config: PathBuf,
        #[arg(long)]
        attempt_id: uuid::Uuid,
        #[arg(long)]
        reason: String,
    },
    /// Replay retained reports for one stopped native attempt; preserves all local state.
    ReconcileNative {
        #[arg(long)]
        native_config: PathBuf,
        #[arg(long)]
        attempt_id: uuid::Uuid,
    },
    /// Pure finite qualification vectors/request checks; never executes a model.
    #[cfg(feature = "test-support")]
    FixtureResponses,
    /// Live-qualification only: serve the runner-held Gemini API-key proxy
    /// until SIGTERM, writing `{origin, capability}` (never the key) to an
    /// owner-only ready file.
    #[cfg(feature = "test-support")]
    GeminiProxy {
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        ready_file: PathBuf,
    },
    /// Print the node id and server (never the credential).
    Status,
}

#[cfg(feature = "synthetic-runtime")]
#[derive(clap::Args)]
struct RunExecutionArgs {
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
}

#[derive(clap::Args)]
struct RunNativeArgs {
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
}

#[derive(clap::Args)]
struct SandboxRunArgs {
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
}

#[derive(clap::Args)]
struct EnrollArgs {
    #[arg(long)]
    server: String,
    /// Worker name (default: the hostname).
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    cf_service_token: bool,
    /// Also store this node's runner Access service token in
    /// runner-access.json next to worker.toml. Never pass it as an argument.
    #[arg(long)]
    runner_access_token: bool,
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
    /// Build, check, scan and publish a Job's project environment recipes; prints one Job result.
    #[cfg(feature = "project_environments")]
    EnvironmentBuild {
        #[arg(long)]
        job: PathBuf,
        /// The project repository, checked out at the Job's commit.
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        build_config: PathBuf,
        /// New output directory for the scanned OCI archives.
        #[arg(long)]
        output: PathBuf,
    },
    /// Build a Job's recipe and run its command in the result; publishes nothing, prints one Job result.
    #[cfg(feature = "project_environments")]
    EnvironmentTry {
        #[arg(long)]
        job: PathBuf,
        /// The task's repository, used as the build context (without `.git`).
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        build_config: PathBuf,
    },
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
    runner: Option<RunnerAccess>,
}

fn secret_reader() -> Reader {
    if std::io::stdin().is_terminal() {
        Box::new(|prompt| Ok(rpassword::prompt_password(prompt)?))
    } else {
        let mut lines = std::io::stdin().lock().lines();
        Box::new(move |what| match lines.next() {
            Some(l) => Ok(l?),
            None => anyhow::bail!("stdin ended before the {what}"),
        })
    }
}

fn read_secrets(cf: bool, runner: bool) -> anyhow::Result<Secrets> {
    read_secrets_from(secret_reader(), cf, runner)
}

/// Only the node's Access client id and secret (a resumed enrollment needs no code).
fn read_access() -> anyhow::Result<(String, String)> {
    let mut next = secret_reader();
    let id = next("Cloudflare Access client id: ")?.trim().to_string();
    let secret = next("Cloudflare Access client secret: ")?
        .trim()
        .to_string();
    anyhow::ensure!(
        !id.is_empty() && !secret.is_empty(),
        "Access client id and secret must not be empty"
    );
    Ok((id, secret))
}

fn read_secrets_from(mut next: Reader, cf: bool, runner: bool) -> anyhow::Result<Secrets> {
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
    let runner = if runner {
        let client_id = next("Runner Access client id: ")?.trim().to_string();
        let client_secret = next("Runner Access client secret: ")?.trim().to_string();
        if client_id.is_empty() || client_secret.is_empty() {
            anyhow::bail!("runner Access client id and secret must not be empty");
        }
        Some(RunnerAccess {
            client_id,
            client_secret,
        })
    } else {
        None
    };
    if code.is_empty() {
        anyhow::bail!("enrollment code is empty");
    }
    Ok(Secrets { code, cf, runner })
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

fn shutdown_watch() -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<bool>,
) {
    let (signal, receiver) = tokio::sync::watch::channel(false);
    let watcher = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = signal.send(true);
    });
    (watcher, receiver)
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

fn claude_docker(config: &aq_worker::native::runner::NativeConfig) -> bool {
    matches!(
        config.executor,
        Some(aq_worker::native::runner::ExecutorConfig::ClaudeDockerLocalResultLocal { .. })
    )
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

#[cfg(feature = "test-support")]
async fn gemini_proxy_cli(
    key_file: &std::path::Path,
    ready_file: &std::path::Path,
) -> anyhow::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let proxy =
        aq_worker::native::runner::sandbox::gemini_proxy::GeminiApiProxy::start(key_file).await?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let ready =
        serde_json::json!({"origin": proxy.origin(), "capability": proxy.local_capability()});
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(ready_file)?;
    file.write_all(&serde_json::to_vec(&ready)?)?;
    file.sync_all()?;
    drop(file);
    tokio::select! {
        _ = term.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
    proxy.stop().await
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let sandbox_agent = if matches!(&cli.command, Cmd::SandboxRun(_)) {
        match aq_session_host::users::bootstrap_sandbox_users() {
            Ok(agent) => Some(agent),
            Err(error) => return usage_err(error),
        }
    } else {
        None
    };
    run(cli, sandbox_agent)
}
fn command_exit<E, M: std::fmt::Display>(
    result: Result<(), E>,
    message: impl FnOnce(E) -> M,
) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => usage_err(message(error)),
    }
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
        Cmd::Template { command } => worker_commands::template(command),
        #[cfg(feature = "synthetic-runtime")]
        Cmd::RunExecution(args) => worker_commands::execution(args, path).await,
        Cmd::SandboxRun(args) => native_commands::sandbox(args, sandbox_agent),
        Cmd::Ms1Definitions => native_commands::definitions(),
        Cmd::SandboxHarness { driver_config } => native_commands::harness(driver_config).await,
        Cmd::CheckNative { native_config } => native_commands::check(path, native_config),
        Cmd::ReleaseNativeSandbox {
            native_config,
            attempt_id,
            reason,
        } => native_commands::release(path, native_config, attempt_id, reason).await,
        Cmd::ReconcileNative {
            native_config,
            attempt_id,
        } => native_commands::reconcile(path, native_config, attempt_id).await,
        Cmd::RunNative(args) => native_commands::run(args, path).await,
        #[cfg(feature = "test-support")]
        Cmd::FixtureResponses => command_exit(
            fixture_responses_cli::run(),
            |_| "finite response fixture input refused",
        ),
        #[cfg(feature = "test-support")]
        Cmd::GeminiProxy {
            key_file,
            ready_file,
        } => command_exit(gemini_proxy_cli(&key_file, &ready_file).await, |error| {
            format!("gemini proxy refused: {error:#}")
        }),
        Cmd::Status => worker_commands::status(path),
        Cmd::BoatVmTimeBackfill {
            controller_config,
            facts,
        } => service_commands::backfill(path, controller_config, facts).await,
        Cmd::RunChecks { checks_config } => service_commands::checks(path, checks_config).await,
        Cmd::Service { command } => command_exit(service::dispatch(command).await, |error| {
            format!("{error:#}")
        }),
        Cmd::HostTerminalRelay { service_config } => command_exit(
            Box::pin(host_terminal::run(&service_config)).await,
            |_| "host terminal companion stopped",
        ),
        Cmd::TerminalRelay { service_config } => command_exit(
            Box::pin(manual_stream::run(&service_config)).await,
            |_| "terminal relay stopped",
        ),
        Cmd::Run {
            controller_config,
            service_config,
        } => service_commands::run(path, controller_config, service_config).await,
        Cmd::Enroll(args) => worker_commands::enroll(args, path).await,
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
fn reconcile_native_requires_an_explicit_preserved_attempt() {
    assert!(
        Cli::try_parse_from([
            "aq-worker",
            "reconcile-native",
            "--native-config",
            "/native.json"
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "aq-worker",
            "reconcile-native",
            "--native-config",
            "/native.json",
            "--attempt-id",
            "2d41bbec-0000-4000-8000-000000000001"
        ])
        .is_ok()
    );
}

#[cfg(test)]
#[test]
fn run_requires_explicit_controller_configuration() {
    assert!(Cli::try_parse_from(["aq-worker", "run"]).is_err());
    assert!(Cli::try_parse_from(["aq-worker", "run", "--controller-config", "/node.json"]).is_ok());
    assert!(Cli::try_parse_from(["aq-worker", "run", "--service-config", "/service.toml"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "aq-worker",
            "run",
            "--controller-config",
            "/node.json",
            "--service-config",
            "/service.toml"
        ])
        .is_err()
    );
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

#[cfg(test)]
fn scripted(lines: &[&str]) -> Reader {
    let mut it: std::collections::VecDeque<String> = lines.iter().map(|s| s.to_string()).collect();
    Box::new(move |what| {
        it.pop_front()
            .ok_or_else(|| anyhow::anyhow!("ran out before {what}"))
    })
}

#[cfg(test)]
#[test]
fn read_secrets_orders_code_cf_runner() {
    let s = read_secrets_from(
        scripted(&["code1 ", "cfid", "cfsecret", " rid", "rsecret"]),
        true,
        true,
    )
    .unwrap();
    assert_eq!(s.code, "code1");
    assert_eq!(s.cf, Some(("cfid".into(), "cfsecret".into())));
    let runner = s.runner.unwrap();
    assert_eq!(
        (runner.client_id.as_str(), runner.client_secret.as_str()),
        ("rid", "rsecret")
    );

    let s = read_secrets_from(scripted(&["code1", "rid", "rsecret"]), false, true).unwrap();
    assert!(s.cf.is_none());
    assert_eq!(s.runner.unwrap().client_id, "rid");

    let s = read_secrets_from(scripted(&["code1"]), false, false).unwrap();
    assert!(s.runner.is_none());

    for bad in [&["code1", "", "rsecret"][..], &["code1", "rid", ""][..]] {
        let e = read_secrets_from(scripted(bad), false, true).err().unwrap();
        assert!(format!("{e:#}").contains("runner Access"), "{e:#}");
    }
}

#[cfg(test)]
#[test]
fn enroll_never_accepts_runner_token_in_argv() {
    let base = ["aq-worker", "enroll", "--server", "http://127.0.0.1:8081"];
    let ok = Cli::try_parse_from(base.iter().chain(&["--runner-access-token"])).unwrap();
    assert!(matches!(
        ok.command,
        Cmd::Enroll(EnrollArgs {
            runner_access_token: true,
            ..
        })
    ));
    assert!(Cli::try_parse_from(base.iter().chain(&["--runner-access-token=abc"])).is_err());
    assert!(Cli::try_parse_from(base.iter().chain(&["--runner-access-token", "abc"])).is_err());
}
