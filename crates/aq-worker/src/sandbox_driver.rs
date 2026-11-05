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
//! 5. copy `/aq/work` back into the worktree, then stop and destroy;
//! 6. submit the AQ result claim through the attempt's local result channel.
//!
//! The token never appears in argv, the container create body, labels, logs
//! or the pane. The driver itself never talks to aq-server.
use crate::backend::docker_local::tar::{TreeEntry, read_tree, tree_archive};
use crate::backend::docker_local::{DockerLocalBackend, DockerLocalConfig};
use crate::backend::{
    Confirmation, CreateRequest, DestroyAuthorization, Ownership, PlacementBackend,
    PrivateMaterial, ResourceLimits, RunnerExit, SandboxIdentity, TemplateDigest,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
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

/// Appended to the startup context inside the sandbox.
pub const RUNNER_NOTE: &str = "\n\nRunner note (docker-local sandbox): AQ_AGENT_RESULT_DIR and \
AQ_AGENT_RESULT_COMMAND are not available here. Do not write request.json or run the result client. \
The node runner submits your final message as the AQ result claim, so end with exactly one line: \
Changes: ...; Checks: exact commands and outcomes; Limits: .... Your worktree is retained as is: keep \
build outputs out of it (for a Rust crate add a .gitignore with /target, or run cargo clean).\n";

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
}

/// Token file contents to the token: one line, trailing CR/LF trimmed,
/// ASCII-graphic only (C6b install rule).
pub fn parse_token(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let token = Zeroizing::new(bytes[..end].to_vec());
    ensure!(
        (16..=4096).contains(&token.len()) && token.iter().all(u8::is_ascii_graphic),
        "Claude login token shape refused"
    );
    Ok(token)
}

fn read_private(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("private input unavailable")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.len() <= max,
        "private input owner/mode/bound"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "private input grew");
    Ok(bytes)
}

fn relative(path: &Path, root: &Path) -> Result<String> {
    let rel = path.strip_prefix(root)?;
    let text = rel.to_str().context("workspace path UTF-8")?;
    ensure!(
        rel.components().all(|c| matches!(c, Component::Normal(_))),
        "workspace path shape"
    );
    Ok(text.to_owned())
}

/// The worktree as archive entries, without `.git` at its root.
pub fn collect_tree(root: &Path, max_bytes: u64) -> Result<Vec<TreeEntry>> {
    use std::os::unix::fs::MetadataExt;
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut children: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let path = child.path();
            if dir == root && child.file_name() == ".git" {
                continue;
            }
            let rel = relative(&path, root)?;
            let meta = std::fs::symlink_metadata(&path)?;
            let mode = meta.mode() & 0o777;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                entries.push(TreeEntry::Symlink {
                    path: rel,
                    target: target.to_str().context("symlink UTF-8")?.into(),
                });
            } else if meta.is_dir() {
                entries.push(TreeEntry::Dir { path: rel, mode });
                stack.push(path);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
                ensure!(
                    total <= max_bytes,
                    "workspace exceeds the sandbox copy bound"
                );
                entries.push(TreeEntry::File {
                    path: rel,
                    mode,
                    data: std::fs::read(&path)?,
                });
            } else {
                bail!("workspace holds an unsupported file type");
            }
        }
    }
    Ok(entries)
}

/// Make `root` (except `.git`) hold exactly `entries`. Writes never follow an
/// existing symlink; removed paths are deleted. Returns the changed paths.
pub fn sync_tree(root: &Path, entries: &[TreeEntry]) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    let mut wanted = BTreeSet::new();
    let git = |p: &str| p == ".git" || p.starts_with(".git/");
    for entry in entries {
        let rel = entry.path();
        ensure!(
            Path::new(rel)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "copy-back path shape"
        );
        if git(rel) {
            continue;
        }
        wanted.insert(rel.to_owned());
    }
    // Remove what the sandbox no longer has (deepest first).
    let current = collect_tree(root, u64::MAX)?;
    let mut stale: Vec<_> = current
        .iter()
        .map(|e| e.path().to_owned())
        .filter(|p| !wanted.contains(p))
        .collect();
    stale.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for rel in stale {
        let path = root.join(&rel);
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        changed.push(rel);
    }
    let current: BTreeMap<String, TreeEntry> = collect_tree(root, u64::MAX)?
        .into_iter()
        .map(|e| (e.path().to_owned(), e))
        .collect();
    for entry in entries {
        let rel = entry.path();
        if git(rel) {
            continue;
        }
        let path = root.join(rel);
        let existing = current.get(rel);
        if existing == Some(entry) {
            continue;
        }
        // Replace any differing kind; never write through a symlink.
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            let same_kind = matches!(
                (entry, existing),
                (TreeEntry::Dir { .. }, Some(TreeEntry::Dir { .. }))
                    | (TreeEntry::File { .. }, Some(TreeEntry::File { .. }))
            );
            if !same_kind {
                if meta.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        match entry {
            TreeEntry::Dir { mode, .. } => {
                std::fs::create_dir_all(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode | 0o700))?;
            }
            TreeEntry::File { mode, data, .. } => {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(*mode)
                    .open(&path)?;
                file.write_all(data)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode | 0o600))?;
            }
            TreeEntry::Symlink { target, .. } => {
                std::os::unix::fs::symlink(target, &path)?;
            }
        }
        changed.push(rel.to_owned());
    }
    Ok(changed)
}

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
        env.push_str(&format!("{k}={v}\n"));
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
    Ok(vec![
        file("prompt", prompt),
        file("args", (config.arguments.join("\n") + "\n").into_bytes()),
        file("env", env.into_bytes()),
        file("settings.json", config.settings_json.clone().into_bytes()),
        file("executable", config.executable.clone().into_bytes()),
    ])
}

/// The last stream-json `result` event Claude printed, if any.
fn last_result(stream: &[u8]) -> Option<serde_json::Value> {
    stream
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|v| v["type"] == "result")
}

/// MS1 review B1: a Claude exit 0 alone never proves the work was done
/// (Claude can end its turn with a question, a refusal or "couldn't find X").
/// The driver records success only when all of these hold; otherwise the
/// executor reports Failed and D3 sends the task to rework.
///
/// - the runner exited 0 (backend-observed);
/// - the last stream-json `result` event has `subtype == "success"` and
///   `is_error == false` (auth failures, max-turns, budget and errors set
///   `is_error`);
/// - the agent's final text contains its own `Changes:` line;
/// - the copied-back change set is non-empty.
pub fn proven_success(
    stream: &[u8],
    rc: Option<i32>,
    changed: &[String],
) -> std::result::Result<(), &'static str> {
    if rc != Some(0) {
        return Err("runner exit not 0");
    }
    let result = last_result(stream).ok_or("no final result event")?;
    if result["subtype"] != "success" || result["is_error"] != false {
        return Err("final result is not a success");
    }
    let text = result["result"].as_str().ok_or("no final message")?;
    if !text
        .lines()
        .any(|line| line.trim_start().starts_with("Changes:"))
    {
        return Err("final message has no Changes: line");
    }
    if changed.is_empty() {
        return Err("no workspace change was copied back");
    }
    Ok(())
}

/// One-line, bounded conclusion from Claude's stream-json `result` event.
pub fn conclusion(stream: &[u8], rc: Option<i32>, changed: &[String]) -> String {
    let mut text = None;
    for line in stream.split(|b| *b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
            && v["type"] == "result"
        {
            text = v["result"].as_str().map(str::to_owned);
        }
    }
    let text = text.unwrap_or_else(|| "no final message".into());
    let summary = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Changes:"))
        .map(|l| l.trim_start().trim_start_matches("Changes:").to_string())
        .unwrap_or(text.clone());
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let files = if changed.is_empty() {
        "none".to_string()
    } else {
        changed.join(",")
    };
    let rc = rc.map_or("unknown".to_string(), |c| c.to_string());
    let mut line = format!("Changes: {files}; Claude exit {rc}; Summary: {summary}");
    if line.len() > 500 {
        let mut cut = 497;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str("...");
    }
    line
}

/// Human-readable pane line for one stream-json event (never tool inputs).
fn pane_line(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    match v["type"].as_str()? {
        "assistant" => {
            let parts: Vec<String> = v["message"]["content"]
                .as_array()?
                .iter()
                .filter_map(|c| match c["type"].as_str()? {
                    "text" => Some(format!("claude: {}", c["text"].as_str()?)),
                    "tool_use" => Some(format!("claude tool: {}", c["name"].as_str()?)),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        "result" => Some(format!(
            "claude result: {} turns={} cost_usd={}",
            v["subtype"].as_str().unwrap_or("?"),
            v["num_turns"],
            v["total_cost_usd"]
        )),
        "system" => Some(format!(
            "claude session {} model {}",
            v["session_id"].as_str().unwrap_or("?"),
            v["model"].as_str().unwrap_or("?")
        )),
        _ => None,
    }
}

/// Prints complete runner lines to the pane. A line holding the token is
/// never printed (a note is); returns whether that happened.
fn print_logs(bytes: &[u8], partial: &mut Vec<u8>, token: &[u8]) -> bool {
    let mut leaked = false;
    partial.extend_from_slice(bytes);
    while let Some(pos) = partial.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = partial.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        if holds_token(line, token) {
            leaked = true;
            println!(
                "aq-sandbox-harness: [a runner output line was withheld: it held the login token]"
            );
            continue;
        }
        if line.starts_with(b"{") {
            if let Some(text) = pane_line(line) {
                println!("{text}");
            }
        } else {
            println!("{}", String::from_utf8_lossy(line));
        }
    }
    let _ = std::io::stdout().flush();
    leaked
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    let config = parsed?;
    let token = parse_token(&raw?)?;
    let session = std::env::var_os("AQ_SESSION_DIRECTORY").context("session directory")?;
    let prompt = std::fs::read(Path::new(&session).join("context-0.bin"))?;
    println!(
        "aq-sandbox-harness: attempt {} template {}",
        config.owner.attempt_id,
        config.template.as_str()
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut docker = config.docker_local.clone();
    docker.runner_command = Some(vec!["/bin/bash".into(), "-c".into(), RUNNER.into()]);
    let mut backend = DockerLocalBackend::connect_unix(docker).await?;
    let request = CreateRequest::new(
        config.owner.clone(),
        config.template.clone(),
        config.hard_deadline_ms,
        vec![],
    )
    .with_resources(config.resources);
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
        &mut term,
        &mut hup,
    )
    .await;
    let (rc, changed, stream, leak) = match outcome {
        Ok(v) => v,
        Err(error) => {
            // Keep the disk: stop and preserve, never destroy unretained work.
            let stopped = backend.stop(&sandbox).await;
            println!(
                "aq-sandbox-harness: preserved sandbox {} after error: {error:#} (stop confirmed: {})",
                sandbox.backend_id,
                stopped.is_confirmed()
            );
            write_exit(config_path, false);
            // Always claim: a recorded (failed) result lets settlement send the
            // task to rework and a replacement attempt run, instead of leaving
            // a stopped attempt with no report holding the task.
            submit_claim(
                &config,
                format!(
                    "Changes: unknown; Runner failure: {error:#}; sandbox {} preserved",
                    sandbox.backend_id
                ),
            );
            return Ok(3);
        }
    };
    drop(token);
    // Non-secret transcript beside the driver config (evidence, measurement),
    // in the private driver directory, and never when it held the token.
    if let Some(dir) = config_path.parent()
        && leak.is_none()
    {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("claude.jsonl"))
            .and_then(|mut f| f.write_all(&stream));
    }
    let stopped = backend.stop(&sandbox).await;
    let destroyed = if stopped.is_confirmed() {
        let authorization = DestroyAuthorization::retained(
            sandbox.clone(),
            format!("node-worktree:{}", config.owner.attempt_id),
        )?;
        matches!(
            backend.destroy(&sandbox, &authorization).await,
            Confirmation::Confirmed(_)
        )
    } else {
        false
    };
    println!(
        "aq-sandbox-harness: sandbox {} stopped={} destroyed={destroyed}; changed: {}",
        sandbox.backend_id,
        stopped.is_confirmed(),
        if changed.is_empty() {
            "none".into()
        } else {
            changed.join(", ")
        }
    );
    // The exit record precedes the claim: the node may stop this pane as soon
    // as the claim is recorded.
    let proven = match leak {
        Some(place) => Err(place),
        None => proven_success(&stream, rc, &changed),
    };
    if let Err(reason) = proven {
        println!(
            "aq-sandbox-harness: success not proven ({reason}); the result is reported as failed"
        );
    }
    write_exit(config_path, proven.is_ok());
    submit_claim(
        &config,
        match leak {
            // Never forward the agent's own text when the token leaked.
            Some(place) => {
                format!("Changes: none (copy-back refused); Refused: {place}; the attempt failed")
            }
            None => conclusion(&stream, rc, &changed),
        },
    );
    Ok(if proven.is_ok() { 0 } else { 1 })
}

/// `exit.json` beside the driver config: success means the runner exited 0
/// (backend-observed) and the workspace was copied back.
fn write_exit(config_path: &Path, success: bool) {
    if let Some(dir) = config_path.parent() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("exit.json"))
            .and_then(|mut f| {
                f.write_all(
                    serde_json::json!({ "success": success })
                        .to_string()
                        .as_bytes(),
                )
            });
    }
}

fn submit_claim(config: &DriverConfig, text: String) {
    let Some(dir) = &config.result_dir else {
        return;
    };
    let mut text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > 500 {
        let mut cut = 497;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
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

/// Return code, copied-back changes, transcript, and where the token leaked.
type Drove = (Option<i32>, Vec<String>, Vec<u8>, Option<&'static str>);

/// Whether `haystack` contains the token's exact bytes.
pub fn holds_token(haystack: &[u8], token: &[u8]) -> bool {
    !token.is_empty()
        && haystack.len() >= token.len()
        && haystack.windows(token.len()).any(|w| w == token)
}

/// MS1 review S3b: the agent's tools can read the token from the claude
/// process environment. Refuse the copy-back (it would reach central
/// retention and, under D1, the remote) when any copied-back file or link,
/// or the transcript, holds the token's exact bytes. Never names the bytes.
pub fn token_leak(entries: &[TreeEntry], stream: &[u8], token: &[u8]) -> Option<&'static str> {
    let in_tree = entries.iter().any(|e| match e {
        TreeEntry::File { path, data, .. } => {
            holds_token(data, token) || holds_token(path.as_bytes(), token)
        }
        TreeEntry::Symlink { path, target, .. } => {
            holds_token(target.as_bytes(), token) || holds_token(path.as_bytes(), token)
        }
        TreeEntry::Dir { path, .. } => holds_token(path.as_bytes(), token),
    });
    if in_tree {
        Some("the Claude login token appeared in the workspace")
    } else if holds_token(stream, token) {
        Some("the Claude login token appeared in the Claude transcript")
    } else {
        None
    }
}

async fn drive(
    backend: &mut DockerLocalBackend,
    config: &DriverConfig,
    sandbox: &SandboxIdentity,
    prompt: &[u8],
    token: &Zeroizing<Vec<u8>>,
    term: &mut tokio::signal::unix::Signal,
    hup: &mut tokio::signal::unix::Signal,
) -> Result<Drove> {
    let workspace = collect_tree(&config.workspace, config.max_workspace_bytes)?;
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
    let exit = loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            _ = term.recv() => bail!("stop requested"),
            _ = hup.recv() => bail!("pane closed"),
        }
        let poll = now_ms() / 1000;
        if let Ok(bytes) = backend.logs_since(sandbox, since, 4 * 1024 * 1024).await {
            pane_leak |= print_logs(&bytes, &mut partial, token);
            since = poll;
        }
        if let Ok(exit) = backend.runner_exit(sandbox).await {
            if let Ok(bytes) = backend.logs_since(sandbox, since, 4 * 1024 * 1024).await {
                pane_leak |= print_logs(&bytes, &mut partial, token);
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
        .get_archive(sandbox, "/aq/out/claude.jsonl", 16 * 1024 * 1024)
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
    let leak = token_leak(&entries, &stream, token)
        .or(pane_leak.then_some("the Claude login token appeared in the runner output"));
    if leak.is_some() {
        // Nothing the sandbox produced is copied back.
        return Ok((rc, Vec::new(), Vec::new(), leak));
    }
    let changed = sync_tree(&config.workspace, &entries)?;
    Ok((rc, changed, stream, None))
}

fn read_tree_file(archive: &[u8]) -> Option<Vec<u8>> {
    crate::backend::docker_local::tar::single_file(archive, 16 * 1024 * 1024).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MS1 review S3b: copy-back and success are refused when the token's
    /// exact bytes appear in a copied-back file, path or link, the transcript
    /// or a pane line (which is then withheld).
    #[test]
    fn token_in_workspace_transcript_or_pane_is_refused() {
        let token: Vec<u8> = [b"fixture-login-", &b"0123456789abcdef"[..]].concat();
        let file = |path: &str, data: &[u8]| TreeEntry::File {
            path: path.into(),
            mode: 0o644,
            data: data.to_vec(),
        };
        let clean = vec![file("src/lib.rs", b"pub fn hello() {}\n")];
        assert_eq!(token_leak(&clean, b"{\"type\":\"result\"}", &token), None);
        let mut leaked = token.clone();
        leaked.splice(0..0, b"export TOKEN=".iter().copied());
        assert!(token_leak(&[file("notes.txt", &leaked)], b"", &token).is_some());
        let name = format!("x{}", String::from_utf8(token.clone()).unwrap());
        assert!(token_leak(&[file(&name, b"ok")], b"", &token).is_some());
        let link = TreeEntry::Symlink {
            path: "l".into(),
            target: String::from_utf8(token.clone()).unwrap(),
        };
        assert!(token_leak(&[link], b"", &token).is_some());
        assert!(token_leak(&clean, &leaked, &token).is_some_and(|w| w.contains("transcript")));
        // A near miss is not the token.
        assert_eq!(token_leak(&clean, &token[..token.len() - 1], &token), None);
        let mut partial = Vec::new();
        let mut lines = b"fine\n".to_vec();
        lines.extend_from_slice(&leaked);
        lines.push(b'\n');
        assert!(print_logs(&lines, &mut partial, &token));
        assert!(!print_logs(b"also fine\n", &mut partial, &token));
    }

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

    #[test]
    fn token_trims_newlines_and_refuses_bad_shapes() {
        assert_eq!(
            &parse_token(b"fixture-login-abcdefghijklmnop\r\n").unwrap()[..],
            b"fixture-login-abcdefghijklmnop"
        );
        assert!(parse_token(b"short\n").is_err());
        assert!(parse_token(b"fixture-login-abc defghijklmnop\n").is_err());
        assert!(parse_token(b"fixture-login-abcdefghijklmnop\nsecond\n").is_err());
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
        };
        assert!(input_entries(&config, b"p").is_err(), "API key env refused");
    }

    #[test]
    fn copy_back_applies_adds_edits_and_deletes_but_never_touches_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "old").unwrap();
        std::fs::write(root.join("gone.txt"), "x").unwrap();
        let mut entries = collect_tree(root, 1 << 20).unwrap();
        assert!(entries.iter().all(|e| !e.path().starts_with(".git")));
        entries.retain(|e| e.path() != "gone.txt");
        for e in &mut entries {
            if let TreeEntry::File { path, data, .. } = e
                && path == "src/lib.rs"
            {
                *data = b"new".to_vec();
            }
        }
        entries.push(TreeEntry::File {
            path: "README.md".into(),
            mode: 0o644,
            data: b"hello".to_vec(),
        });
        entries.push(TreeEntry::File {
            path: ".git/HEAD".into(),
            mode: 0o644,
            data: b"evil".to_vec(),
        });
        let mut changed = sync_tree(root, &entries).unwrap();
        changed.sort();
        assert_eq!(changed, ["README.md", "gone.txt", "src/lib.rs"]);
        assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap(), b"new");
        assert!(!root.join("gone.txt").exists());
        assert_eq!(std::fs::read(root.join(".git/HEAD")).unwrap(), b"ref");
        assert!(sync_tree(root, &entries).unwrap().is_empty(), "idempotent");
    }

    #[test]
    fn success_requires_exit_zero_a_successful_result_a_changes_line_and_changes() {
        let result = |subtype: &str, is_error: bool, text: &str| {
            format!(
                "{{\"type\":\"system\"}}\n{}\n",
                serde_json::json!({"type":"result","subtype":subtype,"is_error":is_error,"result":text})
            )
            .into_bytes()
        };
        let done = result(
            "success",
            false,
            "Did it.\nChanges: src/lib.rs; Checks: cargo test; Limits: none",
        );
        let files = vec!["src/lib.rs".to_string()];
        assert_eq!(proven_success(&done, Some(0), &files), Ok(()));
        // Each refusal case alone keeps the attempt failed.
        assert_eq!(
            proven_success(&done, Some(1), &files),
            Err("runner exit not 0")
        );
        assert_eq!(
            proven_success(&done, None, &files),
            Err("runner exit not 0")
        );
        assert_eq!(
            proven_success(b"{\"type\":\"assistant\"}\n", Some(0), &files),
            Err("no final result event")
        );
        for (subtype, is_error) in [
            ("success", true),
            ("error_max_turns", true),
            ("error_during_execution", false),
        ] {
            assert_eq!(
                proven_success(&result(subtype, is_error, "Changes: x"), Some(0), &files),
                Err("final result is not a success"),
                "{subtype} {is_error}"
            );
        }
        // A clarifying question or refusal ends the turn without a Changes line.
        let question = result(
            "success",
            false,
            "Which file should I edit? I could not find src/lib.rs.",
        );
        assert_eq!(
            proven_success(&question, Some(0), &files),
            Err("final message has no Changes: line")
        );
        assert_eq!(
            proven_success(&done, Some(0), &[]),
            Err("no workspace change was copied back")
        );
        // Only the LAST result event counts.
        let mut later = done.clone();
        later.extend(result("error_during_execution", true, "Changes: none"));
        assert_eq!(
            proven_success(&later, Some(0), &files),
            Err("final result is not a success")
        );
    }

    #[test]
    fn conclusion_is_one_bounded_line() {
        let stream = b"{\"type\":\"system\"}\n{\"type\":\"result\",\"result\":\"Added hello.\\nAll good.\"}\n";
        let line = conclusion(stream, Some(0), &["src/lib.rs".into()]);
        assert_eq!(
            line,
            "Changes: src/lib.rs; Claude exit 0; Summary: Added hello. All good."
        );
        let long = format!("{{\"type\":\"result\",\"result\":\"{}\"}}", "é".repeat(400));
        assert!(conclusion(long.as_bytes(), None, &[]).len() <= 512);
    }
}
