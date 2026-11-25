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
pub mod workspaces;

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
use transport::Scope;
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
        let task = self
            .task_ref
            .strip_prefix("refs/heads/aq/")
            .unwrap_or_default();
        ensure!(
            (self.endpoint == endpoint(self.repository_id)
                || workspaces::scoped_endpoint(&self.endpoint, self.repository_id))
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

/// `/v1/git/<repository_id>` on the worker API origin.
fn endpoint(repository: Uuid) -> String {
    format!("/v1/git/{repository}")
}

/// A full object id, safe for git's argv.
fn oid(v: &str) -> bool {
    v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())
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

/// `refs/heads/aq-try/<attempt>`: where an attempt's `aq environment try`
/// checkpoints go (aq-server spec 0017 §6.2). Never the task ref.
#[cfg(feature = "environment-try")]
pub fn try_ref(attempt: Uuid) -> String {
    format!("refs/heads/aq-try/{attempt}")
}

/// The checkpoint's own worktree, beside the attempt's (outside its tree).
#[cfg(feature = "environment-try")]
fn checkpoint_tree(path: &Path) -> PathBuf {
    path.with_extension("try")
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
    /// Verify the owner-selected commit and clean files before recovering a missing sandbox.
    pub async fn verify_checkout(
        &self,
        attempt: Uuid,
        path: PathBuf,
        expected: String,
    ) -> Result<()> {
        let git = self.git.clone();
        blocking(move || {
            let worktree = NodeGit::open(&path)?;
            ensure!(
                worktree.attempt == attempt
                    && worktree.repository == git.repository(worktree.grant.repository_id),
                "checkout does not belong to this attempt and repository"
            );
            ensure!(
                git.ok(&path, &["rev-parse", "HEAD"], None)?.trim() == expected,
                "retained checkout revision differs"
            );
            ensure!(
                git.ok(
                    &path,
                    &[
                        "status",
                        "--porcelain=v1",
                        "--untracked-files=all",
                        "--ignore-submodules=none"
                    ],
                    None
                )?
                .is_empty(),
                "retained checkout has uncommitted changes"
            );
            Ok(())
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
    /// [`NodeGit::checkpoint`] of the attempt's worktree at `path`. `held`
    /// (the caller's guard) is dropped only when the git work has ended, even
    /// when the caller stopped waiting.
    #[cfg(feature = "environment-try")]
    pub async fn checkpoint(
        &self,
        path: PathBuf,
        message: String,
        author: Option<GitAuthor>,
        fill: impl FnOnce(&Path) -> Result<()> + Send + 'static,
        held: impl Send + 'static,
    ) -> Result<String> {
        let (git, remote) = (self.git.clone(), self.remote.clone());
        blocking(move || {
            let _held = held;
            let worktree = NodeGit::open(&path)?;
            let author = author.as_ref().map(GitAuthor::identity);
            git.checkpoint(&remote, &worktree, &message, author, fill)
        })
        .await
    }
    /// [`NodeGit::checkout`] for an environment job.
    #[cfg(feature = "environment-job")]
    pub async fn checkout(
        &self,
        job: Uuid,
        repository: Uuid,
        commit: String,
        path: PathBuf,
    ) -> Result<Synced> {
        let (git, remote) = (self.git.clone(), self.remote.clone());
        blocking(move || git.checkout(&remote, job, repository, &commit, &path)).await
    }
    /// [`NodeGit::prune`].
    #[cfg(feature = "environment-job")]
    pub async fn prune(&self) -> Result<()> {
        let git = self.git.clone();
        blocking(move || git.prune()).await
    }
    /// [`NodeGit::discard`].
    pub async fn discard(&self, repository: Uuid, path: PathBuf) -> Result<()> {
        let git = self.git.clone();
        blocking(move || git.discard(repository, &path)).await
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
    fn sync(
        &self,
        remote: &Remote,
        (endpoint, scope): (&str, Scope),
        commits: &[&str],
        repo: &Path,
    ) -> Result<Synced> {
        let synced = if repo.join("HEAD").exists() {
            Synced::Fetched
        } else {
            // A clone cut short leaves no HEAD and is started again.
            let _ = std::fs::remove_dir_all(repo);
            std::fs::create_dir_all(repo)?;
            self.ok(repo, &["init", "--bare", "--quiet"], None)?;
            Synced::Cloned
        };
        let url = remote.url(endpoint);
        let refspec = format!("+refs/heads/*:{TRACKING}*");
        let fetch = [
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            "--no-write-fetch-head",
        ];
        let args: Vec<&str> = fetch.into_iter().chain([url.as_str(), &refspec]).collect();
        self.ok(repo, &args, Some((remote, scope)))?;
        for (index, commit) in commits.iter().enumerate() {
            let object = format!("{commit}^{{commit}}");
            if index == 0
                && endpoint.starts_with("/v1/git/workspaces/")
                && let Scope::Attempt(attempt) = scope
                && !self.run(repo, &["cat-file", "-e", &object], None)?.ok
            {
                self.import_workspace_bundle(remote, endpoint, attempt, repo, commit)?;
            }
            self.ok(repo, &["cat-file", "-e", &object], None)
                .context("the commit is not in central aq-git")?;
        }
        Ok(synced)
    }

    fn import_workspace_bundle(
        &self,
        remote: &Remote,
        endpoint: &str,
        attempt: Uuid,
        repo: &Path,
        commit: &str,
    ) -> Result<()> {
        // Called from Node::start_siblings's blocking task with the cache lock
        // held. The temporary file is removed after success or a partial read.
        let bundle = tempfile::NamedTempFile::new_in(&self.repositories)?;
        let path = bundle
            .path()
            .to_str()
            .context("workspace bundle path UTF-8")?;
        tokio::runtime::Handle::try_current()?.block_on(remote.workspace_bundle(
            attempt,
            endpoint,
            bundle.reopen()?,
            self.limits.command_timeout,
        ))?;
        let heads = self.ok(repo, &["bundle", "list-heads", path], None)?;
        ensure!(
            !heads.contains('\n') && heads.split_once(' ').map(|(oid, _)| oid) == Some(commit),
            "workspace bundle differs from the approved commit"
        );
        self.ok(repo, &["bundle", "unbundle", path], None)?;
        Ok(())
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
        let commits: Vec<&str> = std::iter::once(&grant.base)
            .chain(&grant.lease)
            .map(String::as_str)
            .collect();
        let scope = (grant.endpoint.as_str(), Scope::Attempt(attempt));
        let synced = self.sync(remote, scope, &commits, &repo)?;
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
        let commit = self.commit(&worktree.path, message, author)?;
        let grant = &worktree.grant;
        let lease = format!(
            "--force-with-lease={}:{}",
            grant.task_ref,
            grant.lease.as_deref().unwrap_or_default()
        );
        self.push(remote, worktree, &lease, &grant.task_ref, commit)
    }

    /// Commits the whole tree at `dir` on its HEAD, as `workspace.checkpoint
    /// mode:all` did; an unchanged tree is its HEAD.
    fn commit(&self, dir: &Path, message: &str, author: Option<(&str, &str)>) -> Result<String> {
        self.ok(dir, &["add", "--all"], None)?;
        if !self.run(dir, &["diff", "--cached", "--quiet"], None)?.ok {
            self.ok_as(
                dir,
                &["commit", "--quiet", "--no-verify", "-m", message],
                None,
                author,
            )?;
        }
        self.ok(dir, &["rev-parse", "HEAD"], None)
    }

    /// Pushes `commit` to `target` with `force` (a lease or `--force`),
    /// retrying a lost or refused push the same way.
    fn push(
        &self,
        remote: &Remote,
        worktree: &Worktree,
        force: &str,
        target: &str,
        commit: String,
    ) -> Result<Pushed> {
        let url = remote.url(&worktree.grant.endpoint);
        let refspec = format!("{commit}:{target}");
        let args = ["push", "--porcelain", "--no-verify", force, &url, &refspec];
        let mut last = String::new();
        for n in 0..self.limits.push_attempts {
            if n > 0 {
                std::thread::sleep(self.limits.retry_delay);
            }
            let scope = Scope::Attempt(worktree.attempt);
            let out = self.run(&worktree.path, &args, Some((remote, scope)))?;
            // Porcelain: `<flag>\t<from>:<to>\t<summary>`; `=` is "up to date",
            // which is what a retry sees after a push whose reply was lost.
            let ours = |l: &&str| l.split('\t').nth(1).is_some_and(|r| r.ends_with(target));
            match out.stdout.lines().find(ours) {
                Some(l) if out.ok && !l.starts_with('!') => {
                    return Ok(Pushed::Pushed {
                        task_ref: target.to_owned(),
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
            "push to {target} failed {} times: {}",
            self.limits.push_attempts,
            remote.redact(last.trim())
        )
    }

    /// `aq environment try` (aq-server spec 0017 §6.2): the agent's live tree
    /// as a commit on the attempt's HEAD, force-pushed to [`try_ref`]. It is
    /// made as [`NodeGit::finish_as`] makes the result: a second worktree at
    /// HEAD (`<path>.try`), which `fill` makes hold the tree the way the
    /// copy-back does, then the same commit. The attempt's worktree, its index
    /// and HEAD, and the task ref are never touched.
    #[cfg(feature = "environment-try")]
    pub fn checkpoint(
        &self,
        remote: &Remote,
        worktree: &Worktree,
        message: &str,
        author: Option<(&str, &str)>,
        fill: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<String> {
        let (repo, tree) = (&worktree.repository, checkpoint_tree(&worktree.path));
        // One checkpoint per attempt at a time, even one its caller gave up on.
        let one = std::fs::File::open(record(&worktree.path))?;
        one.lock_exclusive()?;
        let head = self.ok(&worktree.path, &["rev-parse", "HEAD"], None)?;
        {
            let _lock = self.lock(worktree.grant.repository_id)?;
            self.remove_worktree(repo, &tree)?;
            let target = tree.to_str().context("worktree path UTF-8")?;
            let add = ["worktree", "add", "--quiet", "--detach", target, &head];
            self.ok(repo, &add, None)?;
        }
        let pushed = fill(&tree).and_then(|()| {
            let commit = self.commit(&tree, message, author)?;
            let target = try_ref(worktree.attempt);
            self.push(remote, worktree, "--force", &target, commit)
        });
        let _lock = self.lock(worktree.grant.repository_id)?;
        self.remove_worktree(repo, &tree)?;
        match pushed? {
            Pushed::Pushed { commit, .. } => Ok(commit),
            Pushed::Conflict => bail!("a forced push met a lease"),
        }
    }

    /// Removes the attempt's worktree and its record, then lets git pack and
    /// prune the shared repository.
    pub fn end(&self, worktree: &Worktree) -> Result<()> {
        #[cfg(feature = "environment-try")]
        self.discard(
            worktree.grant.repository_id,
            &checkpoint_tree(&worktree.path),
        )?;
        self.discard(worktree.grant.repository_id, &worktree.path)?;
        std::fs::remove_file(record(&worktree.path))?;
        Ok(())
    }

    /// An environment job (aq-server spec 0017) at `commit`: fetches the
    /// project's repository as [`NodeGit::start`] does, with the job named in
    /// [`transport::ENVIRONMENT_JOB_HEADER`] instead of an attempt, then adds
    /// a detached worktree at `path`. [`NodeGit::discard`] removes it.
    #[cfg(feature = "environment-job")]
    pub fn checkout(
        &self,
        remote: &Remote,
        job: Uuid,
        repository: Uuid,
        commit: &str,
        path: &Path,
    ) -> Result<Synced> {
        ensure!(oid(commit), "the job's commit is not a full object id");
        ensure!(path.is_absolute(), "worktree path must be absolute");
        let repo = self.repository(repository);
        let _lock = self.lock(repository)?;
        let endpoint = endpoint(repository);
        let scope = (endpoint.as_str(), Scope::EnvironmentJob(job));
        let synced = self.sync(remote, scope, &[commit], &repo)?;
        self.remove_worktree(&repo, path)?;
        std::fs::create_dir_all(path.parent().context("worktree parent")?)?;
        let target = path.to_str().context("worktree path UTF-8")?;
        let add = ["worktree", "add", "--quiet", "--detach", target, commit];
        self.ok(&repo, &add, None)?;
        Ok(synced)
    }

    /// Drops the worktree records whose directories are gone, in every
    /// repository (after a crash left them).
    #[cfg(feature = "environment-job")]
    pub fn prune(&self) -> Result<()> {
        let Ok(entries) = std::fs::read_dir(&self.repositories) else {
            return Ok(());
        };
        for entry in entries {
            let name = entry?.file_name();
            let id = name.to_str().and_then(|n| n.strip_suffix(".git"));
            let Some(id) = id.and_then(|n| Uuid::try_parse(n).ok()) else {
                continue;
            };
            let _lock = self.lock(id)?;
            self.ok(&self.repository(id), &["worktree", "prune"], None)?;
        }
        Ok(())
    }

    /// Removes the worktree at `path`, then lets git pack and prune the
    /// shared repository.
    pub fn discard(&self, repository: Uuid, path: &Path) -> Result<()> {
        let _lock = self.lock(repository)?;
        let repo = self.repository(repository);
        self.remove_worktree(&repo, path)?;
        self.ok(&repo, &["gc", "--auto", "--quiet"], None)?;
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

    fn ok(&self, dir: &Path, args: &[&str], remote: Option<(&Remote, Scope)>) -> Result<String> {
        self.ok_as(dir, args, remote, None)
    }

    fn ok_as(
        &self,
        dir: &Path,
        args: &[&str],
        remote: Option<(&Remote, Scope)>,
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
    fn run(&self, dir: &Path, args: &[&str], remote: Option<(&Remote, Scope)>) -> Result<Output> {
        self.run_as(dir, args, remote, None)
    }

    fn run_as(
        &self,
        dir: &Path,
        args: &[&str],
        remote: Option<(&Remote, Scope)>,
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
        if let Some((remote, scope)) = remote {
            remote.apply(&mut command, scope);
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
