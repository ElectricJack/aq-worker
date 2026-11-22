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
//! When the tool crashes, or is stopped because the claim lease
//! (`build_wall_ms`) is running out, the runner completes the job as `failed`
//! with the redacted tail of the tool's stderr. A job that completed nothing
//! makes the runner back off before it claims again.
#![cfg(feature = "project_environments")]
pub mod client;
pub mod node;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
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
    /// The largest claim or complete reply read; a larger one is refused.
    #[serde(default = "default_reply_bytes")]
    pub max_reply_bytes: u64,
}
fn default_claim_ms() -> u64 {
    5_000
}
/// The default of every reply cap but the admitted list's.
pub fn default_reply_bytes() -> u64 {
    1024 * 1024
}
/// The bounds a reply cap setting is validated against.
pub const REPLY_BYTES: std::ops::RangeInclusive<u64> = 64 * 1024..=256 * 1024 * 1024;

impl EnvironmentsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&read_bounded(path)?)?;
        ensure!(
            REPLY_BYTES.contains(&config.max_reply_bytes),
            "max_reply_bytes must be 64 KiB..=256 MiB"
        );
        Ok(config)
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
    let runner = Runner {
        client: Client::new(server)?,
        node: config.node_git.node(server.remote()),
        config: &config,
        target,
        tool: &tool,
        shutdown: shutdown.clone(),
    };
    // Whatever a killed runner left: job directories (with their archives) and
    // the worktree records of the ones that were checked out.
    std::fs::create_dir_all(&config.work_dir)?;
    for entry in std::fs::read_dir(&config.work_dir)?.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str());
        if name.is_some_and(|n| Uuid::try_parse(n).is_ok())
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            tracing::warn!(path = %path.display(), "leftover environment job not removed: {e}");
        }
    }
    if let Err(e) = runner.node.prune().await {
        tracing::warn!("worktree records of leftover environment jobs not pruned: {e:#}");
    }
    let interval = Duration::from_millis(config.claim_interval_ms);
    let (mut forbidden, mut unfinished) = (0, 0);
    while !*shutdown.borrow() {
        let wait = match runner.client.claim().await {
            Ok(Some(job)) => {
                forbidden = 0;
                if runner.job(job, Instant::now()).await {
                    unfinished = 0;
                    continue;
                }
                // A node that cannot finish jobs must not claim the queue empty.
                unfinished += 1;
                backoff(unfinished)
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

/// The claim lease ends `build_wall_ms` after the server answered the claim.
/// The node stops its work a tenth of that before the lease ends, so that
/// stopping, redaction and the post all finish inside the lease.
struct Lease {
    end: Instant,
    margin: Duration,
}

impl Lease {
    fn new(claimed: Instant, wall: Duration) -> Self {
        Self {
            end: claimed + wall,
            margin: wall / 10,
        }
    }
    /// Between SIGTERM and SIGKILL.
    fn grace(&self) -> Duration {
        self.margin / 10
    }
    /// When the tool is stopped.
    fn tool_stop(&self) -> Instant {
        self.end - self.margin
    }
    /// When redaction is stopped, which leaves two graces for the post.
    fn redact_stop(&self) -> Instant {
        self.end - 2 * self.grace()
    }
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
    /// Runs one claimed job; true when the server recorded its result.
    async fn job(&self, raw: Value, claimed: Instant) -> bool {
        let job: Job = match serde_json::from_value(raw.clone()) {
            Ok(job) => job,
            Err(e) => {
                tracing::error!("unreadable environment job, left to its lease: {e}");
                return false;
            }
        };
        let lease = Lease::new(claimed, Duration::from_millis(job.limits.build_wall_ms));
        let (id, repository) = (job.job_id, job.repository_id);
        let dir = self.config.work_dir.join(id.to_string());
        let tree = dir.join("repo");
        let checkout = (self.node).checkout(id, repository, job.commit.clone(), tree.clone());
        let result = match checkout.await {
            Err(e) => {
                tracing::warn!(job_id = %id, "environment job checkout refused or failed; completing nothing: {e:#}");
                let _ = std::fs::remove_dir_all(&dir);
                return false;
            }
            Ok(_) => self.execute(&job, raw, &dir, &tree, &lease).await,
        };
        if let Err(e) = self.node.discard(repository, tree).await {
            tracing::warn!(job_id = %id, "environment job worktree cleanup failed: {e:#}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(mut result) => {
                fit(&mut result, job.limits.log_bytes);
                self.complete(id, &result, &lease).await
            }
            Err(e) => {
                tracing::error!(job_id = %id, "environment job could not run here; completing nothing: {e:#}");
                false
            }
        }
    }

    /// The tool's Job result. When the tool crashed, printed no result or was
    /// stopped for the lease, a `failed` result is built here instead.
    async fn execute(
        &self,
        job: &Job,
        mut raw: Value,
        dir: &Path,
        tree: &Path,
        lease: &Lease,
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
        let tool = (self.tool)(&args)?;
        let exited = run_tool(tool, &stdout, &stderr, lease.tool_stop(), lease.grace()).await?;
        if exited.is_some_and(|s| s.success())
            && let Ok(result) = serde_json::from_slice::<Value>(&std::fs::read(&stdout)?)
            && result["job_id"] == json!(job.job_id)
        {
            return Ok(result);
        }
        if exited.is_none() {
            let mut log = std::fs::OpenOptions::new().append(true).open(&stderr)?;
            log.write_all(b"\nthe template tool was stopped before build_wall_ms ran out\n")?;
        }
        let log = self.redact(&stderr, job.limits.log_bytes, dir, lease).await;
        let recipe = job.recipes.first().cloned().unwrap_or_default();
        Ok(json!({
            "job_id": job.job_id, "outcome": "failed", "builds": [], "run": null,
            "failure": {"recipe": recipe, "step": "install", "log": log},
        }))
    }

    /// The tail of `log`, redacted by the template tool itself (`redact-log`,
    /// the credential scanner's patterns).
    async fn redact(&self, log: &Path, limit: usize, dir: &Path, lease: &Lease) -> String {
        let redacted = async {
            let output = dir.join("redacted");
            let args = [
                "redact-log".into(),
                "--input".into(),
                log.into(),
                "--limit".into(),
            ];
            let mut args: Vec<OsString> = args.into();
            args.push(limit.to_string().into());
            let tool = (self.tool)(&args)?;
            let errors = dir.join("redact.err");
            let status = run_tool(tool, &output, &errors, lease.redact_stop(), lease.grace());
            ensure!(
                status.await?.is_some_and(|s| s.success()),
                "redact-log failed"
            );
            Ok(std::fs::read_to_string(output)?)
        };
        redacted.await.unwrap_or_else(|e: anyhow::Error| {
            tracing::warn!("environment job log redaction failed: {e:#}");
            "the template tool failed; its log could not be redacted".into()
        })
    }

    /// Posts the result until it is recorded, refused, shut down or the lease
    /// ends (after which another node may hold the job). True when recorded.
    async fn complete(&self, id: Uuid, result: &Value, lease: &Lease) -> bool {
        let body = serde_json::to_vec(result).expect("a JSON value serializes");
        let mut tries = 0;
        loop {
            let sent = tokio::time::timeout_at(lease.end, self.client.complete(&body)).await;
            match sent {
                Ok(Ok(())) => {
                    tracing::info!(job_id = %id, "environment job completed");
                    return true;
                }
                Ok(Err(Refused::Conflict)) => {
                    tracing::warn!(job_id = %id, "another node holds this job's claim now; its result is dropped");
                    return false;
                }
                Ok(Err(Refused::Transient(m))) if !*self.shutdown.borrow() => {
                    tries += 1;
                    tracing::warn!(job_id = %id, "environment job result not sent: {m}; retrying");
                    let next = (Instant::now() + backoff(tries)).min(lease.end);
                    tokio::time::sleep_until(next).await;
                }
                Ok(Err(e)) => {
                    tracing::error!(job_id = %id, "environment job result dropped: {e}");
                    return false;
                }
                Err(_) => {
                    tracing::warn!(job_id = %id, "the claim lease ended before the result was recorded; dropped");
                    return false;
                }
            }
        }
    }
}

/// Runs a tool in its own process group, with its output in files. At
/// `stop`, the group gets SIGTERM, then SIGKILL after `grace`, and the result
/// is `None`.
async fn run_tool(
    (_dir, mut command): Tool,
    stdout: &Path,
    stderr: &Path,
    stop: Instant,
    grace: Duration,
) -> Result<Option<ExitStatus>> {
    command.stdin(Stdio::null()).process_group(0);
    command
        .stdout(std::fs::File::create(stdout)?)
        .stderr(std::fs::File::create(stderr)?);
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    if let Ok(status) = tokio::time::timeout_at(stop, child.wait()).await {
        return Ok(Some(status?));
    }
    if let Some(pid) = child.id() {
        let group = -(pid as libc::pid_t);
        // SAFETY: the child is not reaped until after the SIGKILL, so it still
        // owns its process group (a zombie at worst) and the id is not reused.
        unsafe { libc::kill(group, libc::SIGTERM) };
        tokio::time::sleep(grace).await;
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
    child.wait().await?;
    Ok(None)
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
