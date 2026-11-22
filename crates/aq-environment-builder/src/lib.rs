//! A builder node's runner for environment build and try jobs (aq-server
//! spec 0017 §6.2, §6.3, §6.6).
//!
//! The runner claims a job, checks out its commit in a worktree of the node's
//! repository for the project (aq-node-git), and runs the template tool's
//! `environment-build` or `environment-try`. Then it posts one Job result.
//! Jobs run one at a time and use no attempt slot.
//!
//! When the job's commit cannot be fetched (the claim is gone), the runner
//! completes nothing; the server hands the job out again after its lease.
//! When the tool crashes or runs past `build_wall_ms`, the runner completes
//! the job as `failed` with the redacted tail of the tool's stderr.
pub mod client;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read as _, Seek as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

pub use client::Server;
use client::{Client, Refused};

/// The template tool as a command, with the directory it lives in, which must
/// outlive the run (`aq_worker::template::command`).
pub type Tool = (tempfile::TempDir, std::process::Command);

/// Operator JSON for `aq-worker run-environments`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentsConfig {
    /// The node's `builder.json` (role `template-build`). Its
    /// `target_repository` is used for every job.
    pub build_config: PathBuf,
    /// Scratch space: one `<job_id>` directory per running job.
    pub work_dir: PathBuf,
    /// The node's git configuration, with the same repositories as its slots.
    pub node_git: aq_node_git::Config,
    #[serde(default = "default_claim_ms")]
    pub claim_interval_ms: u64,
}
fn default_claim_ms() -> u64 {
    5_000
}

impl EnvironmentsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&read_bounded(path)?)?)
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    const MAX: u64 = 1 << 20;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    file.take(MAX + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX, "{} is too large", path.display());
    Ok(bytes)
}

/// The fields of a Job that the runner itself reads. The tool validates the rest.
#[derive(Deserialize)]
struct Job {
    job_id: Uuid,
    kind: String,
    repository_id: Uuid,
    commit: String,
    recipes: Vec<String>,
    limits: Limits,
}
#[derive(Deserialize)]
struct Limits {
    build_wall_ms: u64,
    log_bytes: usize,
}

/// Claims and runs jobs until `shutdown`. On shutdown it stops claiming, but
/// it still finishes and posts the job that is running. It returns an error
/// only when the server refuses the node (an error status other than 403).
pub async fn run(
    server: &Server,
    config: EnvironmentsConfig,
    mut shutdown: watch::Receiver<bool>,
    tool: impl Fn(&[OsString]) -> Result<Tool> + Sync,
) -> Result<()> {
    let builder: Value = serde_json::from_slice(&read_bounded(&config.build_config)?)?;
    let target = (builder.get("target_repository").and_then(Value::as_str))
        .context("the builder config has no target_repository")?
        .to_owned();
    std::fs::create_dir_all(&config.work_dir)?;
    let runner = Runner {
        client: Client::new(server)?,
        node: config.node_git.node(server.remote()),
        config: &config,
        target,
        tool: &tool,
        shutdown: shutdown.clone(),
    };
    let interval = Duration::from_millis(config.claim_interval_ms);
    let mut forbidden = 0;
    while !*shutdown.borrow() {
        let wait = match runner.client.claim().await {
            Ok(Some(job)) => {
                forbidden = 0;
                runner.job(job).await;
                continue;
            }
            Ok(None) => interval,
            Err(Refused::Forbidden) => {
                if forbidden == 0 {
                    tracing::warn!(
                        "this node is not in the server's template-build worker group; backing off"
                    );
                }
                forbidden += 1;
                backoff(forbidden)
            }
            Err(Refused::Transient(m)) => {
                tracing::warn!("environment job claim failed: {m}; retrying");
                interval
            }
            Err(e) => bail!("environment job claim refused: {e}"),
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {}
        }
    }
    Ok(())
}

/// 2 s, 4 s, … up to about a minute.
fn backoff(n: u32) -> Duration {
    Duration::from_secs(1 << n.clamp(1, 6))
}

struct Runner<'a, F> {
    client: Client,
    node: aq_node_git::Node,
    config: &'a EnvironmentsConfig,
    target: String,
    tool: &'a F,
    shutdown: watch::Receiver<bool>,
}

impl<F: Fn(&[OsString]) -> Result<Tool> + Sync> Runner<'_, F> {
    async fn job(&self, raw: Value) {
        let job: Job = match serde_json::from_value(raw.clone()) {
            Ok(job) => job,
            Err(e) => return tracing::error!("unreadable environment job, left to its lease: {e}"),
        };
        let started = Instant::now();
        let (id, repository) = (job.job_id, job.repository_id);
        let dir = self.config.work_dir.join(id.to_string());
        let tree = dir.join("repo");
        let _ = std::fs::remove_dir_all(&dir);
        let checkout = (self.node).checkout(id, repository, job.commit.clone(), tree.clone());
        let result = match checkout.await {
            Err(e) => {
                tracing::warn!(job_id = %id, "environment job checkout refused or failed; completing nothing: {e:#}");
                None
            }
            Ok(_) => match self.execute(&job, raw, &dir, &tree, started).await {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::error!(job_id = %id, "environment job could not run here; completing nothing: {e:#}");
                    None
                }
            },
        };
        if let Err(e) = self.node.discard(repository, tree).await {
            tracing::warn!(job_id = %id, "environment job worktree cleanup failed: {e:#}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(mut result) = result {
            fit(&mut result, job.limits.log_bytes);
            self.complete(id, &result).await;
        }
    }

    /// The tool's Job result. When the tool crashed, printed no result or was
    /// stopped at `build_wall_ms` (measured from the claim, like the lease),
    /// a `failed` result is built here instead.
    async fn execute(
        &self,
        job: &Job,
        mut raw: Value,
        dir: &Path,
        tree: &Path,
        started: Instant,
    ) -> Result<Value> {
        let fields = raw.as_object_mut().context("the job is not an object")?;
        // The tool takes exactly the Job fields; the target is this builder's.
        fields.remove("repository_id");
        fields.insert("target_repository".into(), self.target.clone().into());
        let job_path = dir.join("job.json");
        std::fs::write(&job_path, serde_json::to_vec(&raw)?)?;
        let operation = match job.kind.as_str() {
            "build" => "environment-build",
            "try" => "environment-try",
            other => bail!("unknown job kind {other:?}"),
        };
        let mut args: Vec<OsString> = vec![operation.into(), "--job".into(), job_path.into()];
        args.extend(["--repo".into(), tree.into(), "--build-config".into()]);
        args.push(self.config.build_config.clone().into());
        if job.kind == "build" {
            args.extend(["--output".into(), dir.join("output").into()]);
        }
        let (stdout, stderr) = (dir.join("stdout"), dir.join("stderr"));
        let wall = Duration::from_millis(job.limits.build_wall_ms);
        let budget = wall.saturating_sub(started.elapsed());
        let exited = run_tool((self.tool)(&args)?, &stdout, &stderr, budget).await?;
        if exited.is_some_and(|s| s.success())
            && let Ok(result) = serde_json::from_slice::<Value>(&std::fs::read(&stdout)?)
            && result["job_id"] == json!(job.job_id)
        {
            return Ok(result);
        }
        let mut log = tail(&stderr, 2 * job.limits.log_bytes as u64)?;
        if exited.is_none() {
            log.extend_from_slice(b"\nthe template tool was stopped at build_wall_ms\n");
        }
        let log = self.redact(&log, job.limits.log_bytes, dir, wall).await;
        let recipe = job.recipes.first().cloned().unwrap_or_default();
        Ok(json!({
            "job_id": job.job_id, "outcome": "failed", "builds": [], "run": null,
            "failure": {"recipe": recipe, "step": "install", "log": log},
        }))
    }

    /// The template tool's own redaction (`redact-log`, the scanner's patterns).
    async fn redact(&self, log: &[u8], limit: usize, dir: &Path, wall: Duration) -> String {
        let redacted = async {
            let (input, output) = (dir.join("log"), dir.join("redacted"));
            std::fs::write(&input, log)?;
            let args = [
                "redact-log".into(),
                "--input".into(),
                input.into(),
                "--limit".into(),
            ];
            let mut args: Vec<OsString> = args.into();
            args.push(limit.to_string().into());
            let status =
                run_tool((self.tool)(&args)?, &output, &dir.join("redact.err"), wall).await?;
            ensure!(status.is_some_and(|s| s.success()), "redact-log failed");
            Ok(std::fs::read_to_string(output)?)
        };
        redacted.await.unwrap_or_else(|e: anyhow::Error| {
            tracing::warn!("environment job log redaction failed: {e:#}");
            "the template tool failed; its log could not be redacted".into()
        })
    }

    async fn complete(&self, id: Uuid, result: &Value) {
        let body = serde_json::to_vec(result).expect("a JSON value serializes");
        let mut tries = 0;
        loop {
            match self.client.complete(&body).await {
                Ok(()) => return tracing::info!(job_id = %id, "environment job completed"),
                Err(Refused::Conflict) => {
                    return tracing::warn!(job_id = %id, "another node holds this job's claim now; its result is dropped");
                }
                Err(Refused::Transient(m)) if !*self.shutdown.borrow() => {
                    tries += 1;
                    tracing::warn!(job_id = %id, "environment job result not sent: {m}; retrying");
                    tokio::time::sleep(backoff(tries)).await;
                }
                Err(e) => {
                    return tracing::error!(job_id = %id, "environment job result dropped: {e}");
                }
            }
        }
    }
}

/// Runs a tool in its own process group, with its output in files. At
/// `budget`, the whole group is killed and the result is `None`.
async fn run_tool(
    (_dir, mut command): Tool,
    stdout: &Path,
    stderr: &Path,
    budget: Duration,
) -> Result<Option<ExitStatus>> {
    command.stdin(Stdio::null()).process_group(0);
    command
        .stdout(std::fs::File::create(stdout)?)
        .stderr(std::fs::File::create(stderr)?);
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(budget, child.wait()).await {
        Ok(status) => Ok(Some(status?)),
        Err(_) => {
            if let Some(pid) = child.id() {
                // SAFETY: the unreaped child still owns its process group.
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
            child.wait().await?;
            Ok(None)
        }
    }
}

fn tail(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(max)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Trims the free text of a result (`failure.log`, `run.output` and every
/// `toolchain[].output`) to `limit` bytes in total, as the server requires.
/// Each string keeps its tail. Every string gets an equal share, and what a
/// shorter string leaves unused goes to the longer ones.
fn fit(result: &mut Value, limit: usize) {
    let mut texts: Vec<&mut Value> = Vec::new();
    for (key, value) in result.as_object_mut().into_iter().flatten() {
        match (key.as_str(), value) {
            ("failure", Value::Object(f)) => texts.extend(f.get_mut("log")),
            ("run", Value::Object(r)) => texts.extend(r.get_mut("output")),
            ("builds", Value::Array(builds)) => {
                let toolchains = builds.iter_mut().filter_map(|b| b.get_mut("toolchain"));
                for tools in toolchains.filter_map(Value::as_array_mut) {
                    texts.extend(tools.iter_mut().filter_map(|t| t.get_mut("output")));
                }
            }
            _ => {}
        }
    }
    let mut texts: Vec<&mut String> = (texts.into_iter())
        .filter_map(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();
    texts.sort_by_key(|s| s.len());
    let (mut left, count) = (limit, texts.len());
    for (n, text) in texts.into_iter().enumerate() {
        let share = left / (count - n);
        if text.len() > share {
            let mut cut = text.len() - share;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
        }
        left -= text.len();
    }
}

#[cfg(test)]
mod tests;
