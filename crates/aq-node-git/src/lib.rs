//! One local repository per project per node, a git worktree per attempt (spec
//! 0016 §5.3, Epic 0 W2). The first attempt that needs a project clones it from
//! central aq-git; later attempts only fetch. Each attempt works in its own
//! worktree; when it ends the node commits the tree, pushes
//! the task ref under the offer's lease, and removes the worktree.
//!
//! The sandbox never sees any of this: it gets the worktree's tree without
//! `.git` (`aq_sandbox_archive::workspace::collect_tree`). Only the node, outside
//! the sandbox, fetches and pushes, with the node credential and the attempt
//! named on every request; the server applies that attempt's grant.
//!
//! Slots of one node share the repository. Fetch, worktree add/remove and gc
//! take the repository's lock; work inside one worktree needs none.
//! [`NodeGit`] blocks (it runs git); [`Node`] is its async face.
pub mod transport;

use aq_worker_protocol::git_author::GitAuthor;

use anyhow::{Context as _, Result, bail, ensure};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    io::Read as _,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub use transport::Remote;
use uuid::Uuid;

/// Where the fetched heads live in the local repository.
const TRACKING: &str = "refs/remotes/central/";

/// An attempt's git grant, `GET /v1/git/grants/<attempt>` (spec 0016 §5.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// `/v1/git/<repository_id>` on the worker API origin.
    pub endpoint: String,
    pub repository_id: Uuid,
    /// The commit a task without a branch starts from (the retained main head).
    pub base: String,
    /// `refs/heads/aq/<task-id>`, the only ref the attempt pushes.
    pub task_ref: String,
    /// The task ref's head in central aq-git at offer time. Present: the
    /// attempt starts from the task branch; absent: from `base`.
    pub lease: Option<String>,
    /// From the task's latest `returned` delivery decision.
    pub update_from_main: bool,
}

impl Grant {
    /// Every value later reaches git's argv or a path: refuse anything that is
    /// not a full object id or a plain task branch.
    pub fn validate(&self) -> Result<()> {
        let oid = |v: &str| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit());
        let task = self
            .task_ref
            .strip_prefix("refs/heads/aq/")
            .unwrap_or_default();
        ensure!(
            self.endpoint == format!("/v1/git/{}", self.repository_id)
                && oid(&self.base)
                && self.lease.as_deref().is_none_or(oid)
                && !task.is_empty()
                && task
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && !task.starts_with('.')
                && !task.contains("..")
                && !task.ends_with(".lock"),
            "git grant shape"
        );
        Ok(())
    }
    fn branch(&self) -> &str {
        self.task_ref.trim_start_matches("refs/heads/")
    }
}

/// The node's git configuration (the worker service sets it per slot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Node-wide, shared by every slot: `<root>/<repository-id>.git`.
    pub repositories: PathBuf,
    pub git: PathBuf,
    /// Committer for node-created commits, and the author for old offers.
    pub author_name: String,
    pub author_email: String,
    /// Each git command and the grant request are stopped after this.
    pub command_timeout_ms: u64,
    /// Push attempts in total, all under the same lease.
    pub push_attempts: u32,
    pub retry_delay_ms: u64,
}

/// Limits come from the node's configuration, never constants here.
#[derive(Clone, Debug)]
pub struct Limits {
    pub command_timeout: Duration,
    pub push_attempts: u32,
    pub retry_delay: Duration,
}

/// The node's git state: one repository per project under `repositories`.
/// Worktrees live wherever the caller puts them.
pub struct NodeGit {
    git: PathBuf,
    repositories: PathBuf,
    identity: (String, String),
    limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Synced {
    /// The node had no repository for the project and cloned it.
    Cloned,
    Fetched,
}

/// One attempt's worktree. Its record is kept beside it (`<path>.grant.json`,
/// outside the tree the sandbox gets), so a later boot can finish or end it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub attempt: Uuid,
    pub repository: PathBuf,
    pub path: PathBuf,
    pub grant: Grant,
    /// The commit the attempt started from.
    pub start: String,
    pub synced: Synced,
    /// Paths a merge of main left with conflict markers (spec 0016 §3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Pushed {
    /// The task ref is at `commit` in central aq-git.
    Pushed { task_ref: String, commit: String },
    /// The task ref is no longer at the lease: `push_conflict`.
    Conflict,
}

struct Output {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn record(path: &Path) -> PathBuf {
    path.with_extension("grant.json")
}

/// Tell the agent what happened when the node prepared a returned task's tree.
pub fn conflict_note(path: &Path) -> Option<String> {
    let tree = NodeGit::open(path).ok()?;
    if !tree.grant.update_from_main || tree.grant.lease.is_none() {
        return None;
    }
    let mut note = String::from(
        "\n\nAQ integrated the main revision fetched at startup into the retained task branch before preparing this source-only workspace. Verify the supplied tree and report the checks you ran. AQ handles Git publication after handoff.",
    );
    if tree.conflicts.is_empty() {
        note.push_str(" The merge was clean. Report any further edits truthfully, including when none were needed.\n");
    } else {
        note.push_str(&format!(
            " Resolve the conflict markers in: {}.\n",
            tree.conflicts.join(", ")
        ));
    }
    Some(note)
}

/// [`NodeGit`] with the node's [`Remote`], for async callers.
pub struct Node {
    git: std::sync::Arc<NodeGit>,
    remote: Remote,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Node { .. }")
    }
}

impl Config {
    pub fn node(&self, remote: Remote) -> Node {
        let limits = Limits {
            command_timeout: Duration::from_millis(self.command_timeout_ms),
            push_attempts: self.push_attempts,
            retry_delay: Duration::from_millis(self.retry_delay_ms),
        };
        let identity = (self.author_name.clone(), self.author_email.clone());
        let git = NodeGit::new(
            self.git.clone(),
            self.repositories.clone(),
            identity,
            limits,
        );
        Node {
            git: git.into(),
            remote,
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("node git worker failed")?
}

impl Node {
    pub async fn grant(&self, attempt: Uuid, remaining: Duration) -> Result<Option<Grant>> {
        self.remote
            .grant(attempt, remaining.min(self.git.limits.command_timeout))
            .await
    }
    /// Adds the attempt's worktree from the grant already checked by the caller.
    pub async fn start(&self, attempt: Uuid, path: PathBuf, grant: Grant) -> Result<Worktree> {
        self.start_as(attempt, path, grant, None).await
    }
    pub async fn start_as(
        &self,
        attempt: Uuid,
        path: PathBuf,
        grant: Grant,
        author: Option<GitAuthor>,
    ) -> Result<Worktree> {
        let (git, remote) = (self.git.clone(), self.remote.clone());
        blocking(move || {
            git.start_as(
                &remote,
                &grant,
                attempt,
                &path,
                author.as_ref().map(GitAuthor::identity),
            )
        })
        .await
    }
    /// Commits and pushes the attempt's worktree at `path` ([`NodeGit::finish`]).
    pub async fn finish(&self, path: PathBuf, message: String) -> Result<(Worktree, Pushed)> {
        self.finish_as(path, message, None).await
    }
    pub async fn finish_as(
        &self,
        path: PathBuf,
        message: String,
        author: Option<GitAuthor>,
    ) -> Result<(Worktree, Pushed)> {
        let (git, remote) = (self.git.clone(), self.remote.clone());
        blocking(move || {
            let worktree = NodeGit::open(&path)?;
            let pushed = git.finish_as(
                &remote,
                &worktree,
                &message,
                author.as_ref().map(GitAuthor::identity),
            )?;
            Ok((worktree, pushed))
        })
        .await
    }
    /// Removes the attempt's worktree at `path`, if it is still there.
    pub async fn end(&self, path: PathBuf) -> Result<()> {
        let git = self.git.clone();
        blocking(move || match record(&path).exists() {
            true => git.end(&NodeGit::open(&path)?),
            false => Ok(()),
        })
        .await
    }
}

impl NodeGit {
    pub fn new(
        git: PathBuf,
        repositories: PathBuf,
        identity: (String, String),
        limits: Limits,
    ) -> Self {
        Self {
            git,
            repositories,
            identity,
            limits,
        }
    }

    pub fn repository(&self, id: Uuid) -> PathBuf {
        self.repositories.join(format!("{id}.git"))
    }

    /// The project's repository is current: cloned the first time, fetched
    /// (and pruned) after. Every branch of central aq-git is mirrored under
    /// `refs/remotes/central/`; the grant's commits must be among them.
    fn sync(&self, remote: &Remote, grant: &Grant, attempt: Uuid, repo: &Path) -> Result<Synced> {
        let synced = if repo.join("HEAD").exists() {
            Synced::Fetched
        } else {
            // A clone cut short leaves no HEAD and is started again.
            let _ = std::fs::remove_dir_all(repo);
            std::fs::create_dir_all(repo)?;
            self.ok(repo, &["init", "--bare", "--quiet"], None)?;
            Synced::Cloned
        };
        let url = remote.url(grant);
        let refspec = format!("+refs/heads/*:{TRACKING}*");
        let fetch = [
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            "--no-write-fetch-head",
        ];
        let args: Vec<&str> = fetch.into_iter().chain([url.as_str(), &refspec]).collect();
        self.ok(repo, &args, Some((remote, attempt)))?;
        for commit in std::iter::once(&grant.base).chain(&grant.lease) {
            let object = format!("{commit}^{{commit}}");
            self.ok(repo, &["cat-file", "-e", &object], None)
                .context("the grant's commit is not in central aq-git")?;
        }
        Ok(synced)
    }

    /// Syncs the project's repository and adds the attempt's worktree at
    /// `path`: from the task branch head when the grant has
    /// a lease (a returned task), else from the base.
    pub fn start(
        &self,
        remote: &Remote,
        grant: &Grant,
        attempt: Uuid,
        path: &Path,
    ) -> Result<Worktree> {
        self.start_as(remote, grant, attempt, path, None)
    }

    pub fn start_as(
        &self,
        remote: &Remote,
        grant: &Grant,
        attempt: Uuid,
        path: &Path,
        author: Option<(&str, &str)>,
    ) -> Result<Worktree> {
        grant.validate()?;
        ensure!(path.is_absolute(), "worktree path must be absolute");
        let repo = self.repository(grant.repository_id);
        let _lock = self.lock(grant.repository_id)?;
        let synced = self.sync(remote, grant, attempt, &repo)?;
        let from = grant.lease.clone().unwrap_or_else(|| grant.base.clone());
        // A worktree an earlier boot left behind for this attempt is replaced.
        self.remove_worktree(&repo, path)?;
        std::fs::create_dir_all(path.parent().context("worktree parent")?)?;
        let target = path.to_str().context("worktree path UTF-8")?;
        // The predecessor can still have a worktree when the next attempt
        // starts after a handoff. Each worktree owns its HEAD independently.
        let add = ["worktree", "add", "--quiet", "--detach", target, &from];
        self.ok(&repo, &add, None)?;
        let conflicts = match (&grant.lease, grant.update_from_main) {
            (Some(_), true) => self.merge_main(path, grant, author)?,
            _ => vec![],
        };
        let worktree = Worktree {
            attempt,
            repository: repo,
            path: path.to_owned(),
            grant: grant.clone(),
            start: from,
            synced,
            conflicts,
        };
        std::fs::write(record(path), serde_json::to_vec(&worktree)?)?;
        Ok(worktree)
    }

    /// A returned task with `update_from_main`: merges central aq-git's
    /// `refs/heads/main` as just fetched (aq-git keeps it at the synced origin
    /// head; the grant's base is the replaced attempt's, not current main) and
    /// commits the merge. Conflicted files are committed with their markers;
    /// their paths are returned.
    fn merge_main(
        &self,
        dir: &Path,
        grant: &Grant,
        author: Option<(&str, &str)>,
    ) -> Result<Vec<String>> {
        let main = format!("{TRACKING}main");
        self.ok(dir, &["rev-parse", "--verify", "--quiet", &main], None)
            .context("central aq-git has no refs/heads/main")?;
        let message = format!("Merge main into {}", grant.branch());
        let merge = [
            "merge",
            "--quiet",
            "--no-ff",
            "--no-edit",
            "-m",
            &message,
            &main,
        ];
        let merged = self.run_as(dir, &merge, None, author)?;
        if merged.ok {
            return Ok(vec![]);
        }
        let unmerged = ["diff", "--name-only", "-z", "--diff-filter=U"];
        let conflicts: Vec<String> = (self.ok(dir, &unmerged, None)?.split('\0'))
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        ensure!(
            !conflicts.is_empty(),
            "merge of main failed: {}",
            merged.stderr.trim()
        );
        self.ok(dir, &["add", "--all"], None)?;
        self.ok_as(
            dir,
            &["commit", "--quiet", "--no-verify", "--no-edit"],
            None,
            author,
        )?;
        Ok(conflicts)
    }

    /// The worktree [`NodeGit::start`] made at `path`.
    pub fn open(path: &Path) -> Result<Worktree> {
        let worktree: Worktree = serde_json::from_slice(&std::fs::read(record(path))?)?;
        worktree.grant.validate()?;
        ensure!(worktree.path == path, "worktree record moved");
        Ok(worktree)
    }

    /// After the stopped-writer proof and the copy-back (whose secret scan runs
    /// before this): commits the whole tree, as `workspace.checkpoint mode:all`
    /// did, and pushes it to the task ref under the grant's lease. A lost or
    /// refused push is retried with the same lease; a lease mismatch is
    /// [`Pushed::Conflict`]. Never forces.
    pub fn finish(&self, remote: &Remote, worktree: &Worktree, message: &str) -> Result<Pushed> {
        self.finish_as(remote, worktree, message, None)
    }

    pub fn finish_as(
        &self,
        remote: &Remote,
        worktree: &Worktree,
        message: &str,
        author: Option<(&str, &str)>,
    ) -> Result<Pushed> {
        let dir = &worktree.path;
        self.ok(dir, &["add", "--all"], None)?;
        if !self.run(dir, &["diff", "--cached", "--quiet"], None)?.ok {
            self.ok_as(
                dir,
                &["commit", "--quiet", "--no-verify", "-m", message],
                None,
                author,
            )?;
        }
        let commit = self.ok(dir, &["rev-parse", "HEAD"], None)?;
        let grant = &worktree.grant;
        let lease = format!(
            "--force-with-lease={}:{}",
            grant.task_ref,
            grant.lease.as_deref().unwrap_or_default()
        );
        let url = remote.url(grant);
        let refspec = format!("{commit}:{}", grant.task_ref);
        let args = ["push", "--porcelain", "--no-verify", &lease, &url, &refspec];
        let mut last = String::new();
        for n in 0..self.limits.push_attempts {
            if n > 0 {
                std::thread::sleep(self.limits.retry_delay);
            }
            let out = self.run(dir, &args, Some((remote, worktree.attempt)))?;
            // Porcelain: `<flag>\t<from>:<to>\t<summary>`; `=` is "up to date",
            // which is what a retry sees after a push whose reply was lost.
            let ours = |l: &&str| {
                l.split('\t')
                    .nth(1)
                    .is_some_and(|r| r.ends_with(&grant.task_ref))
            };
            match out.stdout.lines().find(ours) {
                Some(l) if out.ok && !l.starts_with('!') => {
                    return Ok(Pushed::Pushed {
                        task_ref: grant.task_ref.clone(),
                        commit,
                    });
                }
                Some(l) if l.starts_with('!') && l.contains("(stale info)") => {
                    return Ok(Pushed::Conflict);
                }
                _ => last = format!("{}{}", out.stdout, out.stderr),
            }
        }
        bail!(
            "push to {} failed {} times: {}",
            grant.task_ref,
            self.limits.push_attempts,
            remote.redact(last.trim())
        )
    }

    /// Removes the attempt's worktree and its record, then lets git pack and
    /// prune the shared repository.
    pub fn end(&self, worktree: &Worktree) -> Result<()> {
        let _lock = self.lock(worktree.grant.repository_id)?;
        let repo = &worktree.repository;
        self.remove_worktree(repo, &worktree.path)?;
        self.ok(repo, &["gc", "--auto", "--quiet"], None)?;
        std::fs::remove_file(record(&worktree.path))?;
        Ok(())
    }

    fn remove_worktree(&self, repo: &Path, path: &Path) -> Result<()> {
        if path.exists() {
            let target = path.to_str().context("worktree path UTF-8")?;
            let remove = ["worktree", "remove", "--force", "--force", target];
            if !self.run(repo, &remove, None)?.ok {
                std::fs::remove_dir_all(path)?;
            }
        }
        self.ok(repo, &["worktree", "prune"], None)?;
        Ok(())
    }

    fn lock(&self, id: Uuid) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.repositories)?;
        let file = std::fs::File::create(self.repositories.join(format!("{id}.lock")))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn ok(&self, dir: &Path, args: &[&str], remote: Option<(&Remote, Uuid)>) -> Result<String> {
        self.ok_as(dir, args, remote, None)
    }

    fn ok_as(
        &self,
        dir: &Path,
        args: &[&str],
        remote: Option<(&Remote, Uuid)>,
        author: Option<(&str, &str)>,
    ) -> Result<String> {
        let out = self.run_as(dir, args, remote, author)?;
        if !out.ok {
            let said = format!("{}{}", out.stdout, out.stderr);
            let said = remote.map_or(said.clone(), |(r, _)| r.redact(&said));
            bail!("git {} failed ({:?}): {}", args[0], out.code, said.trim());
        }
        Ok(out.stdout.trim().to_owned())
    }

    /// Runs the node's git with no inherited configuration, prompts or hooks,
    /// killed with its process group after the command timeout.
    fn run(&self, dir: &Path, args: &[&str], remote: Option<(&Remote, Uuid)>) -> Result<Output> {
        self.run_as(dir, args, remote, None)
    }

    fn run_as(
        &self,
        dir: &Path,
        args: &[&str],
        remote: Option<(&Remote, Uuid)>,
        author: Option<(&str, &str)>,
    ) -> Result<Output> {
        let mut command = Command::new(&self.git);
        command.arg("-C").arg(dir);
        for setting in [
            "credential.helper=",
            "core.hooksPath=/dev/null",
            "http.followRedirects=false",
            "protocol.allow=never",
            "protocol.http.allow=always",
            "protocol.https.allow=always",
            "commit.gpgSign=false",
            "gc.autoDetach=false",
        ] {
            command.args(["-c", setting]);
        }
        let (name, email) = &self.identity;
        let (author_name, author_email) = author.unwrap_or((name, email));
        command
            .args(args)
            .env_clear()
            .env("LC_ALL", "C")
            .env("HOME", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some((remote, attempt)) = remote {
            remote.apply(&mut command, attempt);
        }
        let mut child = command.spawn().context("git could not start")?;
        let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut kept = Vec::new();
                let _ = pipe.read_to_end(&mut kept);
                String::from_utf8_lossy(&kept).into_owned()
            })
        };
        let stdout = drain(Box::new(child.stdout.take().context("git stdout")?));
        let stderr = drain(Box::new(child.stderr.take().context("git stderr")?));
        let deadline = Instant::now() + self.limits.command_timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                // SAFETY: the unreaped child still owns its process group.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                child.wait()?;
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let (stdout, mut stderr) = (
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default(),
        );
        if status.is_none() {
            stderr.push_str("\nkilled after the git command timeout");
        }
        Ok(Output {
            ok: status.is_some_and(|s| s.success()),
            code: status.and_then(|s| s.code()),
            stdout,
            stderr,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;
