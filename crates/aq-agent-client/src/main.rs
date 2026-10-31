use anyhow::{Result, ensure};
use aq_agent_client::client::{AgentClient, Failure, Outcome};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "aq-agent",
    version,
    about = "Scoped AQ agent client; offline data grants no authority"
)]
struct Cli {
    #[arg(long)]
    state_dir: PathBuf,
    /// Optional assertion of the managed private credential binding. No token values.
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Context {
        #[arg(long)]
        cached: bool,
    },
    Graph {
        #[command(subcommand)]
        command: Graph,
    },
    Operation {
        #[command(subcommand)]
        command: Operation,
    },
}
#[derive(Subcommand)]
enum Operation {
    Get { operation_id: Uuid },
}
#[derive(Subcommand)]
enum Graph {
    Get {
        node_id: Uuid,
    },
    Revision {
        node_id: Uuid,
        revision: String,
    },
    History {
        node_id: Uuid,
        #[arg(long, default_value_t = 32)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    Search {
        text: String,
        #[arg(long)]
        kind_filter: Option<String>,
        #[arg(long, default_value_t = 32)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    Context {
        node_id: Uuid,
        #[arg(long, default_value_t = 1)]
        depth: u32,
        #[arg(long, default_value = "both")]
        direction: String,
        #[arg(long, default_value_t = 32)]
        max_nodes: u32,
        #[arg(long, default_value_t = 64)]
        max_edges: u32,
        #[arg(long, default_value_t = 262144)]
        max_bytes: u32,
    },
    Relationships {
        node_id: Uuid,
        #[arg(long, default_value = "both")]
        direction: String,
        #[arg(long = "edge-type", required = true)]
        edge_types: Vec<String>,
        #[arg(long, default_value_t = 32)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Strict complete CommandRequest JSON. Its original operation UUID is durable before send.
    Command {
        #[arg(long)]
        request_file: Option<PathBuf>,
    },
}
fn input(path: Option<PathBuf>) -> Result<Value> {
    let limit = aq_agent_client::contract::MAX_DOCUMENT_BYTES;
    let reader: Box<dyn Read> = match path {
        Some(path) => Box::new(std::fs::File::open(path)?),
        None => Box::new(std::io::stdin()),
    };
    let mut bytes = vec![];
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(aq_agent_client::contract::parse(&bytes)?)
}
async fn run(cli: Cli) -> Result<Outcome> {
    if let Some(file) = cli.token_file {
        ensure!(
            file == cli.state_dir.join("credential.json"),
            "token-file must match managed session binding"
        );
    }
    let mut client = AgentClient::open(&cli.state_dir)?;
    match cli.command {
        Command::Context { cached: true } => Ok(Outcome {
            value: client.cached_context()?,
            exit_code: 0,
        }),
        Command::Context { cached: false } => client.context().await,
        Command::Operation {
            command: Operation::Get { operation_id },
        } => client.operation(operation_id).await,
        Command::Graph {
            command: Graph::Command { request_file },
        } => client.command(input(request_file)?).await,
        Command::Graph { command } => {
            let query = match command {
                Graph::Get { node_id } => json!({"kind":"get","node_id":node_id}),
                Graph::Revision { node_id, revision } => {
                    json!({"kind":"revision","node_id":node_id,"revision":revision})
                }
                Graph::History {
                    node_id,
                    limit,
                    cursor,
                } => json!({"kind":"history","node_id":node_id,"limit":limit,"cursor":cursor}),
                Graph::Search {
                    text,
                    kind_filter,
                    limit,
                    cursor,
                } => {
                    json!({"kind":"search","text":text,"kind_filter":kind_filter,"limit":limit,"cursor":cursor})
                }
                Graph::Context {
                    node_id,
                    depth,
                    direction,
                    max_nodes,
                    max_edges,
                    max_bytes,
                } => {
                    json!({"kind":"context","node_id":node_id,"depth":depth,"direction":direction,"max_nodes":max_nodes,"max_edges":max_edges,"max_bytes":max_bytes})
                }
                Graph::Relationships {
                    node_id,
                    direction,
                    edge_types,
                    limit,
                    cursor,
                } => {
                    json!({"kind":"relationships","node_id":node_id,"direction":direction,"edge_types":edge_types,"limit":limit,"cursor":cursor})
                }
                Graph::Command { .. } => unreachable!(),
            };
            client.query(query).await
        }
    }
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(outcome) => {
            if outcome.exit_code == 14 || outcome.exit_code == 15 {
                eprintln!("{}", outcome.value);
            } else {
                println!("{}", outcome.value);
            }
            outcome.exit_code.into()
        }
        Err(error) => {
            // Never print raw parser, filesystem, credential, URL or response errors.
            if let Some(failure) = error.downcast_ref::<Failure>() {
                eprintln!("{}", serde_json::to_string(failure).unwrap());
                (if failure.code == "authentication_or_authority_refused" {
                    14
                } else {
                    13
                })
                .into()
            } else {
                eprintln!(
                    "{}",
                    json!({"code":"invalid_contract_or_private_state","effect":"not_asserted","retryable":false})
                );
                15.into()
            }
        }
    }
}
