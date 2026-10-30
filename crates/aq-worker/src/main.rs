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
    /// Connect, heartbeat and reconnect until stopped.
    Run,
    /// Print the node id and server (never the credential).
    Status,
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

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = cli.config.unwrap_or_else(default_path);
    match cli.command {
        Cmd::Status => match WorkerConfig::load(&path) {
            Ok(c) => {
                println!("node id: {}", c.node_id);
                println!("server:  {}", c.server_url);
                ExitCode::SUCCESS
            }
            Err(e) => usage_err(format!("{e:#}")),
        },
        Cmd::Run => {
            let cfg = match WorkerConfig::load(&path) {
                Ok(c) => c,
                Err(e) => return usage_err(format!("{e:#}")),
            };
            let client = match Client::from_config(&cfg) {
                Ok(c) => c,
                Err(e) => return usage_err(format!("{e:#}")),
            };
            tracing::info!(node_id = %cfg.node_id, server = %cfg.server_url, "worker starting");
            match run_loop(&client, shutdown_signal()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_for(&e)
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
