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
//! 4. stream the runner log to the pane and wait for the runner;
//! 5. copy `/aq/work` back into the worktree, then stop the sandbox;
//! 6. submit the AQ result claim through the attempt's local result channel; the node then
//!    destroys the stopped sandbox. A copy-back that fails part way claims nothing.
//!
//! The token never appears in argv, the container create body, labels, logs
//! or the pane. The driver itself never talks to aq-server: with
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
pub use aq_handoff_note::{
    CopyBackIncomplete, HANDOFF_MARKER, HandoffMarker, PRESERVED_MARKER, conclusion, conclusion_of,
    fold_claim, handoff_note, holds_secret, holds_token, parse_token, proven_success,
    read_handoff_marker,
};
use aq_handoff_note::{Secrets, print_logs};
use aq_handoff_note::{final_message, now_ms, preserved_claim, read_private, write_exit};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;

/// Fixed in-sandbox runner (I4a pattern). Public configuration, no secret.
pub const RUNNER: &str = r#"set -uo pipefail
umask 077
in=/aq/in; out=/aq/out; m=/aq/private/material
mkdir -p "$out"
[ -s "$m" ] || { echo "aq-runner: material missing"; exit 90; }
token="$(cat "$m")"; rm -f "$m"
[ -e "$m" ] && { echo "aq-runner: material not wiped"; exit 91; }
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done < "$in/env"
mkdir -p "$HOME" && cp "$in/settings.json" "$HOME/settings.json" || exit 92
set --; while IFS= read -r a; do set -- "$@" "$a"; done < "$in/args"
exe="$(cat "$in/executable")"
cd /aq/work || exit 93
echo "aq-runner: claude $("$exe" --version 2>/dev/null) uid=$(id -u) capeff=$(awk '/^CapEff:/ {print $2}' /proc/self/status) nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 "$exe" "$@" < "$in/prompt" 2> "$out/claude.err" | tee "$out/claude.jsonl"
rc=${PIPESTATUS[0]}
unset token
echo "$rc" > "$out/rc"
login_file=false; [ -e "$HOME/.credentials.json" ] && login_file=true
echo "aq-runner: claude exit $rc login_file=$login_file"
exit "$rc"
"#;

pub use aq_codex_mapper::docker_local::CODEX_RUNNER;

pub use aq_handoff_note::harness::DriverHarness;
/// The fixed in-sandbox runner of a harness.
fn runner(harness: DriverHarness) -> &'static str {
    match harness {
        DriverHarness::Claude => RUNNER,
        DriverHarness::Codex => CODEX_RUNNER,
    }
}

/// Appended to the startup context inside the sandbox.
pub const RUNNER_NOTE: &str = "\n\nRunner note (docker-local sandbox): AQ_AGENT_RESULT_DIR and AQ_AGENT_RESULT_COMMAND are not available here. Do not write request.json or run the \
result client. The node runner submits your final message as the AQ result claim, so end with exactly one line: Changes: ...; Checks: exact commands \
and outcomes; Limits: ... (write Changes: none when you changed no file), and above it one line Outcome: done, or Outcome: blocked when you could not \
do the task. Your worktree is retained as is: keep build outputs out of it (for a Rust crate add a .gitignore with /target, or run cargo clean).\n";
/// Appended when the attempt has an agent channel.
pub const CHANNEL_NOTE: &str = "The aq CLI (/opt/aq/bin/aq, where installed) reaches AQ from this \
sandbox through AQ_CHANNEL; no credential is needed or accepted. `aq commands` lists what you may run.\n";

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
    /// The harness in the sandbox; absent (Claude) in a Claude configuration.
    #[serde(default, skip_serializing_if = "DriverHarness::is_claude")]
    pub harness: DriverHarness,
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
    let file = |path: &str, data: Vec<u8>| TreeEntry::File {
        path: path.into(),
        mode: 0o600,
        data,
    };
    // The startup context tells the agent to run the AQ result client; inside
    // the sandbox the node driver submits the claim from the final message.
    let mut prompt = prompt.to_vec();
    prompt.extend_from_slice(RUNNER_NOTE.as_bytes());
    if config.agent_channel {
        prompt.extend_from_slice(CHANNEL_NOTE.as_bytes());
    }
    Ok(vec![
        file("prompt", prompt),
        file("args", (config.arguments.join("\n") + "\n").into_bytes()),
        file("env", env.into_bytes()),
        file("settings.json", config.settings_json.clone().into_bytes()),
        file("executable", config.executable.clone().into_bytes()),
    ])
}

pub struct DriverOutcome {
    pub sandbox: SandboxIdentity,
    pub runner_exit: Option<RunnerExit>,
    pub changed: Vec<String>,
    pub destroyed: bool,
}

/// Entrypoint for `aq-worker sandbox-harness`. Exit code 0 only when Claude
/// exited 0, the workspace was copied back and the claim was submitted.
pub async fn run(config_path: &Path) -> Result<i32> {
    run_with(config_path, None, DockerLocalBackend::connect_unix).await
}

/// `run` with the backend connection given (the scripted Docker double in tests)
/// and, when `session` is set, that session directory instead of
/// `AQ_SESSION_DIRECTORY`.
pub async fn run_with<T, C, F>(
    config_path: &Path,
    session: Option<&Path>,
    connect: C,
) -> Result<i32>
where
    T: DockerTransport,
    C: FnOnce(DockerLocalConfig) -> F,
    F: std::future::Future<Output = std::result::Result<DockerLocalBackend<T>, BackendError>>,
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
    if config.conversation_outbox.is_some() && config.harness.is_claude() {
        config
            .arguments
            .retain(|arg| arg != "--no-session-persistence");
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
    )
    .await;
    let Drove {
        rc,
        changed,
        stream,
        transcript,
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
            .and_then(|mut f| f.write_all(transcript.as_deref().unwrap_or(&stream)));
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

/// Return code, copied-back changes, transcript, where the token leaked, and
/// the reason when the service asked for a handoff.
struct Drove {
    rc: Option<i32>,
    changed: Vec<String>,
    /// The final-result stream (for Codex, mapped from its own output).
    stream: Vec<u8>,
    /// The harness's own output when it differs from `stream` (Codex).
    transcript: Option<Vec<u8>>,
    leak: Option<&'static str>,
    handoff: Option<aq_protocol::HandoffReason>,
}

#[allow(clippy::too_many_arguments)]
async fn drive<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    config: &DriverConfig,
    sandbox: &SandboxIdentity,
    prompt: &[u8],
    token: &Zeroizing<Vec<u8>>,
    login: &[Zeroizing<Vec<u8>>],
    term: &mut tokio::signal::unix::Signal,
    hup: &mut tokio::signal::unix::Signal,
    handoff: &Path,
) -> Result<Drove> {
    let workspace = collect_tree(&config.workspace, config.max_workspace_bytes)?;
    let tokens = login.iter().map(|t| t.as_slice()).collect();
    let secrets = &Secrets::of_tokens(tokens, workspace.iter().flat_map(tree_bytes));
    backend
        .put_archive(
            sandbox,
            tree_archive("aq/in", &input_entries(config, prompt)?)?,
        )
        .await?;
    backend
        .put_archive(sandbox, tree_archive("aq/work", &workspace)?)
        .await?;
    backend
        .deliver_private_material(sandbox, PrivateMaterial::new(token.to_vec()))
        .await?;
    backend.start_runner(sandbox).await?;
    println!("aq-sandbox-harness: runner started");
    let mut since = now_ms() / 1000;
    let mut partial = Vec::new();
    let mut pane_leak = false;
    let mut handoff_reason = None;
    let exit = loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            _ = term.recv() => bail!("stop requested"),
            _ = hup.recv() => bail!("pane closed"),
        }
        let poll = now_ms() / 1000;
        if let Ok(bytes) = backend.logs_since(sandbox, since, 4 * 1024 * 1024).await {
            pane_leak |= print_logs(&bytes, &mut partial, secrets);
            since = poll;
        }
        if let Some(reason) = read_handoff_marker(handoff) {
            handoff_reason = Some(reason);
            // A stopped container still serves get_archive for the copy-back.
            let _ = backend.stop(sandbox).await;
            break RunnerExit::Signalled;
        }
        if let Ok(exit) = backend.runner_exit(sandbox).await {
            if let Ok(bytes) = backend.logs_since(sandbox, since, 4 * 1024 * 1024).await {
                pane_leak |= print_logs(&bytes, &mut partial, secrets);
            }
            break exit;
        }
        if now_ms() >= config.hard_deadline_ms {
            bail!("sandbox deadline reached");
        }
    };
    let rc = match exit {
        RunnerExit::Exited(code) => Some(code),
        RunnerExit::Signalled => None,
    };
    let stream = backend
        .get_archive(sandbox, config.harness.stream_path(), 16 * 1024 * 1024)
        .await
        .ok()
        .and_then(|a| read_tree_file(&a))
        .unwrap_or_default();
    let archive = backend
        .get_archive(
            sandbox,
            "/aq/work",
            usize::try_from(config.max_workspace_bytes)?.saturating_mul(2),
        )
        .await
        .context("workspace copy-back")?;
    let entries = read_tree(&archive, config.max_workspace_bytes)?;
    let leak = token_leak(&entries, &stream, secrets).or(pane_leak
        .then_some("a secret (login token or AQ credential) appeared in the runner output"));
    if leak.is_some() {
        // Nothing the sandbox produced is copied back.
        return Ok(Drove {
            rc,
            changed: Vec::new(),
            stream: Vec::new(),
            transcript: None,
            leak,
            handoff: None,
        });
    }
    let changed = sync_tree(&config.workspace, &entries)?;
    let (stream, transcript) = match config.harness {
        DriverHarness::Claude => (stream, None),
        DriverHarness::Codex => (
            aq_codex_mapper::docker_local::result_stream(&stream),
            Some(stream),
        ),
    };
    Ok(Drove {
        rc,
        changed,
        stream,
        transcript,
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
            "/aq/home/projects/-aq-work",
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

fn read_tree_file(archive: &[u8]) -> Option<Vec<u8>> {
    crate::backend::docker_local::tar::single_file(archive, 16 * 1024 * 1024).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MS1 review S3a: the token file is read and removed before anything that
    /// can fail, even when the driver config itself is unreadable.
    #[tokio::test]
    async fn token_file_is_removed_even_when_the_driver_config_is_bad() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("driver.json");
        let token = dir.path().join("token");
        for (path, data) in [
            (&config, &b"{not a config"[..]),
            (&token, b"fixture-login-0123456789"),
        ] {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .write_all(data)
                .unwrap();
        }
        assert!(run(&config).await.is_err());
        assert!(!token.exists(), "the leased token must not stay on disk");
    }

    /// 0006 §5.4/§8: no channel socket, no launch; the env names the socket;
    /// an `aqa_` credential anywhere that leaves the sandbox is a leak.
    #[test]
    fn channel_is_required_named_in_env_and_its_credential_guarded() {
        let root = tempfile::tempdir().unwrap();
        let mut config: DriverConfig = serde_json::from_value(serde_json::json!({
            "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
            "template": format!("sha256:{}", "a".repeat(64)),
            "owner": {"node_id": uuid::Uuid::from_u128(1), "boot_id": uuid::Uuid::from_u128(2),
                "attempt_id": uuid::Uuid::from_u128(3)},
            "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
            "workspace": "/w", "token_file": "/t", "executable": "/e", "arguments": [], "environment": {},
            "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1, "agent_channel": true,
        }))
        .unwrap();
        let error = channel_ready(&config).unwrap_err().to_string();
        assert!(error.starts_with("agent_channel_unavailable"), "{error}");
        config.docker_local.channel_root = Some(root.path().into());
        let dir = root.path().join(uuid::Uuid::from_u128(3).to_string());
        std::fs::create_dir(&dir).unwrap();
        assert!(
            channel_ready(&config).is_err(),
            "a directory without a socket"
        );
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("aq.sock")).unwrap();
        channel_ready(&config).unwrap();
        config.environment = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let inputs = input_entries(&config, b"p").unwrap();
        let text = |name: &str| match inputs.iter().find(|e| e.path() == name) {
            Some(TreeEntry::File { data, .. }) => String::from_utf8(data.clone()).unwrap(),
            _ => panic!("{name}"),
        };
        let env = "PATH=/opt/aq/bin:/usr/bin:/bin\nAQ_CHANNEL=unix:/aq/channel/aq.sock\n";
        assert_eq!(text("env"), env);

        assert!(text("prompt").ends_with(CHANNEL_NOTE));
        let credential = format!("aqa_{}", "Q".repeat(43));
        let file = |data: &str| TreeEntry::File {
            path: "f".into(),
            mode: 0o644,
            data: data.into(),
        };
        let token = &Secrets::new(b"fixture-login-0123456789abcdef", None);
        assert!(token_leak(&[file(&credential)], b"", token).is_some());
        assert!(token_leak(&[], credential.as_bytes(), token).is_some());
        assert!(token_leak(&[file("starts_with(\"aqa_\")")], b"", token).is_none());
        let line = format!("x {credential}\n");
        assert!(print_logs(line.as_bytes(), &mut Vec::new(), token));
        // A shape the base tree already held (a fixture in the repository) is no leak.
        let kept = &Secrets::new(b"fixture-login-0123456789abcdef", [credential.as_bytes()]);
        assert!(token_leak(&[file(&credential)], credential.as_bytes(), kept).is_none());
        assert!(token_leak(&[file(&format!("{credential}x"))], b"", kept).is_some());
    }

    #[test]
    fn runner_keeps_the_token_out_of_argv_and_wipes_material_first() {
        let read = RUNNER.find("token=\"$(cat \"$m\")\"").unwrap();
        let wipe = RUNNER.find("rm -f \"$m\"").unwrap();
        let exec = RUNNER.find("CLAUDE_CODE_OAUTH_TOKEN=\"$token\"").unwrap();
        assert!(read < wipe && wipe < exec);
        assert!(!RUNNER.contains("export CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!RUNNER.contains("--dangerously-skip-permissions"));
        assert!(!RUNNER.contains("echo \"$token"));
        let config = DriverConfig {
            docker_local: DockerLocalConfig::new("/run/aq-sandbox/podman.sock".into()),
            template: TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            owner: Ownership {
                node_id: uuid::Uuid::new_v4(),
                boot_id: uuid::Uuid::new_v4(),
                attempt_id: uuid::Uuid::new_v4(),
            },
            hard_deadline_ms: 1,
            resources: ResourceLimits {
                cpu_millis: 1000,
                memory_bytes: 1 << 30,
                pids: 64,
                disk_bytes: None,
            },
            workspace: "/w".into(),
            token_file: "/t".into(),
            executable: "/opt/aq/bin/claude".into(),
            arguments: vec!["-p".into()],
            environment: [("ANTHROPIC_API_KEY".to_string(), "x".to_string())].into(),
            settings_json: "{}".into(),
            result_dir: None,
            max_workspace_bytes: 1,
            agent_channel: false,
            conversation_outbox: None,
            harness: Default::default(),
        };
        assert!(input_entries(&config, b"p").is_err(), "API key env refused");
    }
}
