//! MS1: node-side driver for one Claude Code attempt in a docker-local sandbox.
//!
//! The native pipeline owns the attempt: grant, renewal, Started/Stopped, the
//! result channel, source capture and the report. Its session host starts this
//! driver as the pane program. The driver owns exactly one sandbox:
//!
//! 1. create it through the C2 fixed launch policy (`DockerLocalBackend`);
//! 2. upload the hydrated aq-git worktree (without `.git`) to `/aq/work` and the
//!    non-secret launch inputs (prompt, C6a settings and argv) to `/aq/in`;
//! 3. deliver the leased Claude token as private material; the fixed runner
//!    reads it into `CLAUDE_CODE_OAUTH_TOKEN` for the `claude` process only and
//!    deletes the file before Claude starts;
//! 4. attach the sandbox's terminal to this pane (the harness's interactive TUI:
//!    the owner watches it and types into it) and wait for the turn to end: the
//!    harness's stop hook leaves its final message in `/aq/out/stop.json`;
//! 5. stop the sandbox, then copy `/aq/work` back into the worktree;
//! 6. submit the AQ result claim through the attempt's local result channel; the node then
//!    destroys the stopped sandbox. A copy-back that fails part way claims nothing.
//!
//! The token never appears in argv, the container create body, labels or logs,
//! and the pane output is redacted before it is written (`pane::Relay`). The driver itself never talks to aq-server: with
//! `agent_channel` the node's per-attempt channel (spec 0006 §5, crate
//! `aq-agent-channel`) holds the AQ credential, and the driver only mounts its
//! socket directory at `/aq/channel` and sets `AQ_CHANNEL`. No channel socket,
//! no launch (`agent_channel_unavailable`).
use crate::backend::docker_local::tar::{TreeEntry, read_tree, tree_archive};
use crate::backend::docker_local::{
    DockerLocalBackend, DockerLocalConfig, DockerTransport, MISSING_TEMPLATE,
};
use crate::backend::{
    BackendError, CreateRequest, Ownership, PlacementBackend, PrivateMaterial, ResourceLimits,
    RunnerExit, SandboxIdentity, TemplateDigest,
};
use anyhow::{Context, Result, bail, ensure};
use aq_handoff_note::Secrets;
pub use aq_handoff_note::harness::{CHANNEL_NOTE, CLAUDE_RUNNER as RUNNER, RUNNER_NOTE};
use aq_handoff_note::harness::{STOP_MARKER, stop_stream};
pub use aq_handoff_note::{
    CopyBackIncomplete, HANDOFF_MARKER, HandoffMarker, PRESERVED_MARKER, conclusion, conclusion_of,
    fold_claim, handoff_note, holds_secret, holds_token, parse_token, proven_success,
    read_handoff_marker,
};
use aq_handoff_note::{final_message, now_ms, preserved_claim, read_private, write_exit};
pub use aq_sandbox_archive::pane::{self, Pane};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;

pub use aq_codex_mapper::docker_local::CODEX_RUNNER;

pub use aq_handoff_note::harness::DriverHarness;
/// The fixed in-sandbox runner of a harness.
fn runner(harness: DriverHarness) -> &'static str {
    match harness {
        DriverHarness::Claude => RUNNER,
        DriverHarness::Codex => CODEX_RUNNER,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    pub docker_local: DockerLocalConfig,
    pub template: TemplateDigest,
    pub owner: Ownership,
    /// Unix milliseconds; the sandbox carries this deadline.
    pub hard_deadline_ms: u64,
    pub resources: ResourceLimits,
    /// The hydrated aq-git worktree on the node.
    pub workspace: PathBuf,
    /// Runner-private 0600 file holding the leased token; deleted after read.
    pub token_file: PathBuf,
    /// In-container Claude executable and the C6a recipe for it.
    pub executable: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub settings_json: String,
    /// Local result channel (`AQ_AGENT_RESULT_DIR`); absent means no claim.
    pub result_dir: Option<PathBuf>,
    pub max_workspace_bytes: u64,
    /// Mount the node's channel for this attempt; launch fails without it.
    #[serde(default)]
    pub agent_channel: bool,
    /// Worker-private outbox. The sandbox can write only its own provider file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_outbox: Option<PathBuf>,
    /// The node's limits on the transcript mount; past them the attempt is stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_bound: Option<aq_handoff_note::TranscriptBound>,
    /// The harness in the sandbox; absent (Claude) in a Claude configuration.
    #[serde(default, skip_serializing_if = "DriverHarness::is_claude")]
    pub harness: DriverHarness,
    /// The node's pinned Codex helper bundle, uploaded beside the launch inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_runtime_bundle: Option<PathBuf>,
    /// This task's warm build cache, when the node keeps one for the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cache: Option<aq_sandbox_archive::build_cache::SlotCache>,
}

impl DriverConfig {
    pub fn apply_git_author(
        &mut self,
        author: &aq_worker_protocol::git_author::GitAuthor,
    ) -> Result<()> {
        author.validate()?;
        if matches!(self.harness, DriverHarness::Codex) {
            aq_codex_mapper::apply_tool_environment(
                &mut self.arguments,
                &mut self.environment,
                author.tool_environment(),
            )?;
        } else {
            self.environment
                .extend(author.tool_environment().map(|(k, v)| (k.into(), v.into())));
        }
        Ok(())
    }
}

mod drive;
mod outcome;
pub(crate) mod projects;
use aq_sandbox_archive::workspace::tree_bytes;
pub use aq_sandbox_archive::workspace::{collect_tree, sync_tree, token_leak};

/// The non-secret runner inputs under `/aq/in`.
pub fn input_entries(config: &DriverConfig, prompt: &[u8]) -> Result<Vec<TreeEntry>> {
    for a in &config.arguments {
        ensure!(!a.contains('\n') && !a.contains('\0'), "argument shape");
    }
    let env = input_environment(config)?;
    let file = |path: &str, data: Vec<u8>| TreeEntry::File {
        path: path.into(),
        mode: 0o600,
        data,
    };
    // The startup context tells the agent to run the AQ result client; inside
    // the sandbox the node driver submits the claim from the final message.
    let mut prompt = prompt.to_vec();
    prompt.extend(
        aq_node_git::conflict_note(&config.workspace)
            .unwrap_or_default()
            .bytes(),
    );
    prompt.extend_from_slice(projects::note(&config.workspace)?.as_bytes());
    prompt.extend_from_slice(RUNNER_NOTE.as_bytes());
    if config.agent_channel {
        prompt.extend_from_slice(CHANNEL_NOTE.as_bytes());
    }
    let bundle = config.codex_runtime_bundle.as_deref();
    let mut entries = bundle
        .map(aq_sandbox_archive::codex_runtime::bundle)
        .transpose()?
        .unwrap_or_default();
    entries.extend([
        file("prompt", prompt),
        file("args", (config.arguments.join("\n") + "\n").into_bytes()),
        file("env", env.into_bytes()),
        file("settings.json", config.settings_json.clone().into_bytes()),
        file("executable", config.executable.clone().into_bytes()),
    ]);
    Ok(entries)
}

pub struct DriverOutcome {
    pub sandbox: SandboxIdentity,
    pub runner_exit: Option<RunnerExit>,
    pub changed: Vec<String>,
    pub destroyed: bool,
}

/// Entrypoint for `aq-worker sandbox-harness`. Exit code 0 only when the harness
/// finished its turn, the workspace was copied back and the claim was submitted.
pub async fn run(config_path: &Path) -> Result<i32> {
    let terminal = |socket: PathBuf, path: String| async move {
        Ok((pane::attach_unix(&socket, &path).await?, Pane::stdio()))
    };
    run_with(
        config_path,
        None,
        DockerLocalBackend::connect_unix,
        terminal,
    )
    .await
}

/// `run` with the backend connection and the terminal (the sandbox's attached
/// terminal stream and the pane to relay it to) given, for the scripted Docker
/// double in tests; when `session` is set, that session directory instead of
/// `AQ_SESSION_DIRECTORY`.
pub async fn run_with<T, C, F, A, AF, S>(
    config_path: &Path,
    session: Option<&Path>,
    connect: C,
    terminal: A,
) -> Result<i32>
where
    T: DockerTransport,
    C: FnOnce(DockerLocalConfig) -> F,
    F: std::future::Future<Output = std::result::Result<DockerLocalBackend<T>, BackendError>>,
    A: FnOnce(PathBuf, String) -> AF,
    AF: std::future::Future<Output = Result<(S, Pane)>>,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (config, login) = read_launch(config_path)?;
    let prompt = read_prompt(session)?;
    println!(
        "aq-sandbox-harness: attempt {} template {}",
        config.owner.attempt_id,
        config.template.as_str()
    );
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut signals = drive::Signals { term, hup };
    let (docker, request, cache, workspace_budget) = config.sandbox_request()?;
    let mut backend = connect(docker).await.context("connect sandbox daemon")?;
    let sandbox = match backend.create(&request).await {
        Ok(s) => s,
        Err(MISSING_TEMPLATE) => {
            backend
                .pull_template(&config.template)
                .await
                .context("pull sandbox template")?;
            backend.create(&request).await.context("create sandbox")?
        }
        Err(e) => return Err(e).context("create sandbox"),
    };
    println!("aq-sandbox-harness: sandbox {} created", sandbox.backend_id);
    let handoff = config_path.with_file_name(HANDOFF_MARKER);
    let mut drive = drive::Drive {
        backend: &mut backend,
        config: &config,
        sandbox: &sandbox,
        login: &login,
        handoff: &handoff,
        runner_may_have_started: false,
        cache: cache.as_ref(),
        workspace_budget,
    };
    let drove = match drive.run(&prompt, terminal, &mut signals).await {
        Ok(v) => v,
        Err(error) => return drive.preserve(error, config_path).await,
    };
    let note = drove.handoff_note(&login.redact);
    if drove.leak.is_none() {
        save_conversation(&mut backend, &config, &sandbox, &login.token).await;
    }
    drop(login);
    drove.write_transcript(config_path, &config);
    let stopped = backend.stop(&sandbox).await;
    // The node destroys the stopped sandbox once it sees the claim: the host stop may kill
    // this driver right after the claim, and an unclaimed attempt keeps its sandbox for salvage.
    let code = drove.claim(config_path, &config, note);
    println!(
        "aq-sandbox-harness: sandbox {} stopped={}; changed: {}",
        sandbox.backend_id,
        stopped.is_confirmed(),
        if drove.changed.is_empty() {
            "none".into()
        } else {
            drove.changed.join(", ")
        }
    );
    Ok(code)
}

/// 0006 §5.4: the node's channel socket must exist before the sandbox does.
pub fn channel_ready(config: &DriverConfig) -> Result<()> {
    let root = config.docker_local.channel_root.as_deref();
    aq_agent_channel::socket_ready(root, config.owner.attempt_id)?;
    println!(
        "aq-sandbox-harness: agent channel at {}",
        aq_agent_channel::AQ_CHANNEL
    );
    Ok(())
}

fn submit_claim(config: &DriverConfig, text: String) {
    let Some(dir) = &config.result_dir else {
        return;
    };
    let text = fold_claim(text);
    let result = (|| -> Result<u8> {
        let input = aq_agent_result_local::SubmissionInput {
            schema_version: 1,
            conclusion: aq_agent_result_local::Conclusion::new(text)?,
        };
        let client = aq_agent_client::completion::ResultClient::open(dir)?;
        Ok(client.submit(input)?.exit_code)
    })();
    match result {
        Ok(code) => println!("aq-sandbox-harness: result claim submitted (exit {code})"),
        Err(error) => println!("aq-sandbox-harness: result claim refused: {error:#}"),
    }
}

/// Return code, copied-back changes, final result, where a secret leaked, and
/// the reason when the service asked for a handoff.
struct Drove {
    rc: Option<i32>,
    changed: Vec<String>,
    /// The final `result` event, from the harness's stop hook.
    stream: Vec<u8>,
    leak: Option<&'static str>,
    handoff: Option<aq_handoff_note::Stop>,
}

async fn save_conversation<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    config: &DriverConfig,
    sandbox: &SandboxIdentity,
    token: &[u8],
) {
    let Some(directory) = &config.conversation_outbox else {
        return;
    };
    // Codex runs `--ephemeral`: it keeps no session to save.
    if !config.harness.is_claude() {
        return;
    }
    let attempt = config.owner.attempt_id;
    let captured = tokio::time::timeout(
        Duration::from_secs(2),
        crate::conversation::read_snapshot(
            backend,
            sandbox,
            crate::conversation::Provider::Claude,
            config.harness.transcript_dir(),
            Some(attempt),
            Some(token),
        ),
    )
    .await;
    match captured {
        Ok(Ok(Some(snapshot))) => {
            let saved = serde_json::to_vec(&snapshot)
                .map_err(anyhow::Error::from)
                .and_then(|body| {
                    aq_node_service::enrollment_file::write_private(
                        &directory.join(format!("{attempt}.json")),
                        &body,
                    )
                });
            if saved.is_err() {
                tracing::warn!(attempt_id = %attempt, "could not queue provider conversation");
            }
        }
        Ok(Ok(None)) => {}
        _ => tracing::warn!(attempt_id = %attempt, "could not read provider conversation"),
    }
}

fn input_environment(config: &DriverConfig) -> Result<String> {
    let mut env = String::new();
    for (k, v) in &config.environment {
        ensure!(
            !crate::native::adapter::claude::FORBIDDEN_ENVIRONMENT.contains(&k.as_str())
                && !k.contains('=')
                && !k.is_empty()
                && !v.contains('\n'),
            "Claude environment refused"
        );
        let v = match (config.agent_channel, k.as_str()) {
            (true, "PATH") => aq_agent_channel::with_aq_bin(v),
            _ => v.clone(),
        };
        env.push_str(&format!("{k}={v}\n"));
    }
    if config.agent_channel {
        env.push_str(&format!("AQ_CHANNEL={}\n", aq_agent_channel::AQ_CHANNEL));
    }
    for (name, target) in config.build_cache.iter().flat_map(|c| c.environment()) {
        env.push_str(&format!("{name}={target}\n"));
    }
    Ok(env)
}

fn read_prompt(session: Option<&Path>) -> Result<Vec<u8>> {
    let session = match session {
        Some(session) => session.as_os_str().to_owned(),
        None => std::env::var_os("AQ_SESSION_DIRECTORY").context("session directory")?,
    };
    Ok(std::fs::read(Path::new(&session).join("context-0.bin"))?)
}
struct Login {
    token: Zeroizing<Vec<u8>>,
    redact: Vec<Zeroizing<Vec<u8>>>,
}
fn read_launch(config_path: &Path) -> Result<(DriverConfig, Login)> {
    // MS1 review S3a: the leased token leaves the disk before anything else
    // can fail: read and unlink it first, then parse the rest.
    let parsed = read_private(config_path, 1024 * 1024)
        .and_then(|bytes| Ok(serde_json::from_slice::<DriverConfig>(&bytes)?));
    let token_file = match &parsed {
        Ok(config) => config.token_file.clone(),
        Err(_) => config_path.with_file_name("token"),
    };
    let raw = read_private(&token_file, 64 * 1024);
    let _ = std::fs::remove_file(&token_file);
    let mut config = parsed?;
    let named = config.conversation_outbox.is_some() || config.docker_local.transcript.is_some();
    if named && config.harness.is_claude() {
        config.arguments.extend([
            "--session-id".to_owned(),
            config.owner.attempt_id.to_string(),
        ]);
    }
    // The private material and the secrets that must never leave the sandbox.
    let (token, secrets) = match config.harness {
        DriverHarness::Claude => {
            let token = parse_token(&raw?)?;
            let secrets = vec![token.clone()];
            (token, secrets)
        }
        DriverHarness::Codex => {
            let raw = raw?;
            aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&raw)?;
            let secrets = aq_handoff_note::harness::codex_login_tokens(&raw)?;
            (raw, secrets)
        }
    };
    Ok((
        config,
        Login {
            token,
            redact: secrets,
        },
    ))
}

impl DriverConfig {
    fn sandbox_request(
        &self,
    ) -> Result<(
        DockerLocalConfig,
        CreateRequest,
        Option<aq_sandbox_archive::build_cache::Opened>,
        u64,
    )> {
        let mut docker = self.docker_local.clone();
        let workspace_budget = projects::stage_readonly(
            &self.workspace,
            self.max_workspace_bytes,
            docker.workspace_bind_paths.as_ref(),
            &mut docker.readonly_projects,
        )?;
        docker.runner_tty = true;
        docker.runner_command = Some(vec![
            "/bin/bash".into(),
            "-c".into(),
            runner(self.harness).into(),
        ]);
        let mut request = CreateRequest::new(
            self.owner.clone(),
            self.template.clone(),
            self.hard_deadline_ms,
            vec![],
        )
        .with_resources(self.resources);
        if self.agent_channel {
            channel_ready(self)?;
            request = request.with_agent_channel();
        }
        let cache = (self.build_cache.as_ref()).and_then(|c| c.open(&self.workspace));
        docker.cache_binds = cache.iter().flat_map(|c| c.binds()).collect();
        Ok((docker, request, cache, workspace_budget))
    }
}
