use anyhow::{Context as _, Result, ensure};
use aq_agent_client::client::{AgentClient, Failure, MessageRoute, Outcome};
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
    state_dir: Option<PathBuf>,
    /// Optional assertion of the managed private credential binding. No token values.
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// One bounded local completion claim; no Graph/task-completion authority.
    Result {
        #[command(subcommand)]
        command: ResultCommand,
    },
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
    /// Bound task/session mailbox reads, scoped sends and replies; no harness delivery.
    Message {
        #[command(subcommand)]
        command: Message,
    },
}
#[derive(Subcommand)]
enum Message {
    /// Read the bound session mailbox (or the task mailbox with --task).
    Inbox {
        #[arg(long)]
        task: bool,
        #[arg(long, default_value = "0")]
        after: String,
        #[arg(long, default_value_t = 32)]
        limit: u8,
        #[arg(long)]
        thread: Option<Uuid>,
    },
    /// Strict MessageSend JSON with a caller-chosen operation UUID.
    Send {
        #[arg(long)]
        request_file: Option<PathBuf>,
    },
    /// Strict MessageAck JSON for a message in the bound mailbox.
    Ack {
        #[arg(long)]
        request_file: Option<PathBuf>,
    },
    /// Strict MessageBodyRequest JSON for an omitted body, or every chunk of
    /// one message with --message-id (session mailbox, or task with --task).
    Body {
        #[arg(long, conflicts_with_all = ["message_id", "task"])]
        request_file: Option<PathBuf>,
        #[arg(long)]
        message_id: Option<Uuid>,
        #[arg(long, requires = "message_id")]
        task: bool,
    },
    /// Reply on the original thread to the original sender's mailbox. The body
    /// is read from --body-file or stdin; pass the same IDs to retry.
    Reply {
        #[arg(long)]
        message_id: Uuid,
        #[arg(long)]
        task: bool,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long)]
        operation_id: Option<Uuid>,
        #[arg(long)]
        reply_id: Option<Uuid>,
    },
}
#[derive(Subcommand)]
enum ResultCommand {
    /// Publish the original local intent, normally exit10 (pending).
    Submit {
        #[arg(long)]
        request_file: PathBuf,
    },
    /// Read local worker acknowledgment/recovery data; triggers no effects.
    Status,
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
fn body_input(path: Option<PathBuf>) -> Result<String> {
    let limit = aq_agent_client::mail::MAX_BODY_BYTES;
    let reader: Box<dyn Read> = match path {
        Some(path) => Box::new(std::fs::File::open(path)?),
        None => Box::new(std::io::stdin()),
    };
    let mut bytes = vec![];
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}
async fn run(cli: Cli) -> Result<Outcome> {
    let Cli {
        state_dir,
        token_file,
        command,
    } = cli;
    let command = match command {
        Command::Result { command } => {
            ensure!(
                token_file.is_none(),
                "local result channel accepts no Graph token assertion"
            );
            let root = state_dir
                .or_else(|| {
                    std::env::var_os(aq_agent_result_local::RESULT_DIR_ENV).map(PathBuf::from)
                })
                .context("result runtime channel unavailable")?;
            let client = aq_agent_client::completion::ResultClient::open(&root)?;
            return match command {
                ResultCommand::Submit { request_file } => {
                    client.submit(aq_agent_client::completion::read_input(&request_file)?)
                }
                ResultCommand::Status => client.status(),
            };
        }
        command => command,
    };
    let state_dir = state_dir.context("Graph state-dir required")?;
    if let Some(file) = token_file {
        ensure!(
            file == state_dir.join("credential.json"),
            "token-file must match managed session binding"
        );
    }
    let mut client = AgentClient::open(&state_dir)?;
    match command {
        Command::Result { .. } => {
            unreachable!("local result dispatch completed before Graph client")
        }
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
        Command::Message {
            command:
                Message::Reply {
                    message_id,
                    task,
                    body_file,
                    operation_id,
                    reply_id,
                },
        } => {
            let body = body_input(body_file)?;
            let operation_id = operation_id.unwrap_or_else(Uuid::new_v4);
            let reply_id = reply_id.unwrap_or_else(Uuid::new_v4);
            client
                .reply(!task, message_id, body, operation_id, reply_id)
                .await
        }
        Command::Message {
            command:
                Message::Body {
                    message_id: Some(message_id),
                    task,
                    ..
                },
        } => client.body(!task, message_id).await,
        Command::Message { command } => {
            let (route, request) = match command {
                Message::Inbox {
                    task,
                    after,
                    limit,
                    thread,
                } => (
                    MessageRoute::Inbox,
                    client.inbox_request(!task, &after, limit, thread),
                ),
                Message::Send { request_file } => (MessageRoute::Send, input(request_file)?),
                Message::Ack { request_file } => (MessageRoute::Ack, input(request_file)?),
                Message::Body { request_file, .. } => (MessageRoute::Body, input(request_file)?),
                Message::Reply { .. } => unreachable!("reply dispatched above"),
            };
            client.message(route, request).await
        }
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
