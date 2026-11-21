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
use crate::backend::docker_local::{DockerLocalBackend, DockerLocalConfig, DockerTransport};
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
    io::Write,
    os::unix::fs::OpenOptionsExt,
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

use aq_sandbox_archive::workspace::tree_bytes;
pub use aq_sandbox_archive::workspace::{collect_tree, sync_tree, token_leak};

/// The non-secret runner inputs under `/aq/in`.
pub fn input_entries(config: &DriverConfig, prompt: &[u8]) -> Result<Vec<TreeEntry>> {
    for a in &config.arguments {
        ensure!(!a.contains('\n') && !a.contains('\0'), "argument shape");
    }
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
    let session = match session {
        Some(session) => session.as_os_str().to_owned(),
        None => std::env::var_os("AQ_SESSION_DIRECTORY").context("session directory")?,
    };
    let prompt = std::fs::read(Path::new(&session).join("context-0.bin"))?;
    println!(
        "aq-sandbox-harness: attempt {} template {}",
        config.owner.attempt_id,
        config.template.as_str()
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut docker = config.docker_local.clone();
    docker.runner_tty = true;
    docker.runner_command = Some(vec![
        "/bin/bash".into(),
        "-c".into(),
        runner(config.harness).into(),
    ]);
    let mut request = CreateRequest::new(
        config.owner.clone(),
        config.template.clone(),
        config.hard_deadline_ms,
        vec![],
    )
    .with_resources(config.resources);
    if config.agent_channel {
        channel_ready(&config)?;
        request = request.with_agent_channel();
    }
    let cache = (config.build_cache.as_ref()).and_then(|c| c.open(&config.workspace));
    docker.cache_binds = cache.iter().flat_map(|c| c.binds()).collect();
    let mut backend = connect(docker).await?;
    let sandbox = match backend.create(&request).await {
        Ok(s) => s,
        Err(_) => {
            backend.pull_template(&config.template).await?;
            backend.create(&request).await?
        }
    };
    println!("aq-sandbox-harness: sandbox {} created", sandbox.backend_id);
    let outcome = drive(
        &mut backend,
        &config,
        &sandbox,
        &prompt,
        &token,
        &secrets,
        &mut term,
        &mut hup,
        &config_path.with_file_name(HANDOFF_MARKER),
        terminal,
        cache.as_ref(),
    )
    .await;
    let Drove {
        rc,
        changed,
        stream,
        leak,
        handoff,
    } = match outcome {
        Ok(v) => v,
        Err(error) => {
            // Keep the disk: stop and preserve, never destroy unretained work.
            let stopped = backend.stop(&sandbox).await;
            println!(
                "aq-sandbox-harness: preserved sandbox {} after error: {error:#} (stop confirmed: {})",
                sandbox.backend_id,
                stopped.is_confirmed()
            );
            save_conversation(&mut backend, &config, &sandbox, &token).await;
            write_exit(config_path, false);
            // The marker precedes the claim: the node never destroys a preserved sandbox.
            let Some(claim) = preserved_claim(&error, &sandbox.backend_id, config_path) else {
                println!(
                    "aq-sandbox-harness: no claim (incomplete copy-back or no preserved marker); salvage copies the preserved sandbox again"
                );
                return Ok(5);
            };
            submit_claim(&config, claim);
            return Ok(3);
        }
    };
    let note = match (handoff, leak) {
        (Some(reason), None) => Some(handoff_note(reason, &changed, &stream, &secrets[0])),
        _ => None,
    };
    // A login of several secrets: a note that holds any other one keeps no agent text.
    let note = note.map(|text| match (handoff, secrets.get(1..)) {
        (Some(reason), Some(rest)) if rest.iter().any(|t| holds_token(text.as_bytes(), t)) => {
            handoff_note(reason, &changed, &[], &[])
        }
        _ => text,
    });
    if leak.is_none() {
        save_conversation(&mut backend, &config, &sandbox, &token).await;
    }
    drop(token);
    drop(secrets);
    // Non-secret transcript beside the driver config (evidence),
    // in the private driver directory, and never when it held the token.
    if let Some(dir) = config_path.parent()
        && leak.is_none()
    {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(config.harness.transcript_file()))
            .and_then(|mut f| f.write_all(&stream));
    }
    let stopped = backend.stop(&sandbox).await;
    // The node destroys the stopped sandbox once it sees the claim: the host stop may kill
    // this driver right after the claim, and an unclaimed attempt keeps its sandbox for salvage.
    let code = claim_outcome(config_path, &config, note, leak, &stream, rc, &changed);
    println!(
        "aq-sandbox-harness: sandbox {} stopped={}; changed: {}",
        sandbox.backend_id,
        stopped.is_confirmed(),
        if changed.is_empty() {
            "none".into()
        } else {
            changed.join(", ")
        }
    );
    Ok(code)
}

/// Writes the exit record, then the claim (the node may stop this pane as soon as the claim
/// is recorded), and returns the driver's exit code.
fn claim_outcome(
    config_path: &Path,
    config: &DriverConfig,
    note: Option<String>,
    leak: Option<&'static str>,
    stream: &[u8],
    rc: Option<i32>,
    changed: &[String],
) -> i32 {
    if let Some(note) = note {
        // The work is in the node worktree; the executor sees a recorded
        // failure and the pipeline checkpoints, retains and reports it.
        write_exit(config_path, false);
        submit_claim(config, note);
        return 4;
    }
    let proven = match leak {
        Some(place) => Err(place),
        None => proven_success(stream, rc, changed),
    };
    if let Err(reason) = proven {
        println!(
            "aq-sandbox-harness: success not proven ({reason}); the result is reported as failed"
        );
    }
    write_exit(config_path, proven.is_ok());
    let on = config.agent_channel && leak.is_none();
    let root = &config.docker_local.channel_root;
    let posting = on && root.is_some();
    submit_claim(
        config,
        match leak {
            // Never forward the agent's own text when the token leaked.
            Some(place) => {
                format!("Changes: none (copy-back refused); Refused: {place}; the attempt failed")
            }
            None if config.harness.is_claude() => conclusion(stream, rc, changed, posting),
            None => conclusion_of(config.harness.label(), stream, rc, changed, posting),
        },
    );
    // R2-M2: after the claim, best effort and bounded, so it never delays the result.
    final_message::post_from_stream(on, root, config.owner.attempt_id, stream);
    if proven.is_ok() { 0 } else { 1 }
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

#[allow(clippy::too_many_arguments)]
async fn drive<T, A, AF, S>(
    backend: &mut DockerLocalBackend<T>,
    config: &DriverConfig,
    sandbox: &SandboxIdentity,
    prompt: &[u8],
    token: &Zeroizing<Vec<u8>>,
    login: &[Zeroizing<Vec<u8>>],
    term: &mut tokio::signal::unix::Signal,
    hup: &mut tokio::signal::unix::Signal,
    handoff: &Path,
    terminal: A,
    cache: Option<&aq_sandbox_archive::build_cache::Opened>,
) -> Result<Drove>
where
    T: DockerTransport,
    A: FnOnce(PathBuf, String) -> AF,
    AF: std::future::Future<Output = Result<(S, Pane)>>,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let workspace = collect_tree(&config.workspace, config.max_workspace_bytes)?;
    let tokens = login.iter().map(|t| t.as_slice()).collect();
    let secrets = &Secrets::of_tokens(tokens, workspace.iter().flat_map(tree_bytes));
    backend
        .put_archive(
            sandbox,
            tree_archive("aq/in", &input_entries(config, prompt)?)?,
        )
        .await?;
    let image = config.docker_local.template_image(&config.template);
    let daemon = (config.docker_local.socket.as_path(), image);
    let archive = aq_sandbox_archive::build_cache::work_archive(cache, &workspace, daemon).await?;
    backend.put_archive(sandbox, archive).await?;
    backend
        .deliver_private_material(sandbox, PrivateMaterial::new(token.to_vec()))
        .await?;
    backend.start_runner(sandbox).await?;
    println!("aq-sandbox-harness: runner started");
    // The sandbox's terminal on this pane, from here until the sandbox stops.
    let id = &sandbox.backend_id;
    let attach = format!(
        "{}/containers/{id}/attach?stream=1&stdin=1&stdout=1&stderr=1",
        crate::backend::docker_local::API_VERSION
    );
    let (stream, pane) = terminal(config.docker_local.socket.clone(), attach)
        .await
        .context("sandbox terminal attach")?;
    let relay = pane::Relay::start(stream, pane, login.to_vec());
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    // A resize also makes a TUI that started before the attach paint again.
    let mut size = pane::window_size();
    let mut handoff_reason = None;
    let (exit, stream) = loop {
        if let Some(size) = size.take() {
            let _ = backend.resize(sandbox, size).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            _ = resized.recv() => { size = pane::window_size(); continue; },
            _ = term.recv() => bail!("stop requested"),
            _ = hup.recv() => bail!("pane closed"),
        }
        let mount = (config.docker_local.transcript.as_ref()).zip(config.transcript_bound);
        let over = || mount.and_then(|((directory, _), bound)| bound.exceeded(directory));
        let over = || over().or_else(|| cache.and_then(|c| c.exceeded()));
        if let Some(reason) = read_handoff_marker(handoff).map(Into::into).or_else(over) {
            handoff_reason = Some(reason);
            // A stopped container still serves get_archive for the copy-back.
            let _ = backend.stop(sandbox).await;
            break (RunnerExit::Signalled, Vec::new());
        }
        // The harness's stop hook: the turn ended and this is its final message.
        // The interactive harness does not exit by itself, so the turn's end is
        // the attempt's end.
        let turn = backend.read_file(sandbox, STOP_MARKER).await;
        if let Some(stream) = turn.ok().as_deref().and_then(stop_stream) {
            let _ = backend.stop(sandbox).await;
            break (RunnerExit::Exited(0), stream);
        }
        // The harness left by itself (owner exit, crash): no final message.
        if let Ok(exit) = backend.runner_exit(sandbox).await {
            break (exit, Vec::new());
        }
        if now_ms() >= config.hard_deadline_ms {
            bail!("sandbox deadline reached");
        }
    };
    let pane_leak = relay.finish().await;
    // Below the TUI's last screen, so the driver's own lines do not overwrite it.
    print!("\r\n\x1b[J");
    let rc = match exit {
        RunnerExit::Exited(code) => Some(code),
        RunnerExit::Signalled => None,
    };
    let archive = backend
        .get_archive(
            sandbox,
            "/aq/work",
            usize::try_from(config.max_workspace_bytes)?.saturating_mul(2),
        )
        .await
        .context("workspace copy-back")?;
    let entries = read_tree(&archive, config.max_workspace_bytes)?;
    let leak = token_leak(&entries, &stream, secrets)
        .or(pane_leak.then_some("a login secret appeared in the sandbox terminal output"));
    if leak.is_some() {
        // Nothing the sandbox produced is copied back.
        return Ok(Drove {
            rc,
            changed: Vec::new(),
            stream: Vec::new(),
            leak,
            handoff: None,
        });
    }
    let changed = sync_tree(&config.workspace, &entries)?;
    Ok(Drove {
        rc,
        changed,
        stream,
        leak: None,
        handoff: handoff_reason,
    })
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
