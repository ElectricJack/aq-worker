//! One local repository per project per node, a git worktree per attempt (spec
//! 0016 §5.3, Epic 0 W2). The first attempt that needs a project clones it from
//! central aq-git; later attempts only fetch. Each attempt works in its own
//! worktree on its task branch; when it ends the node commits the tree, pushes
//! the task ref under the offer's lease, and removes the worktree.
//!
//! The sandbox never sees any of this: it gets the worktree's tree without
//! `.git` (`aq_sandbox_archive::workspace::collect_tree`). Only the node, outside
//! the sandbox, fetches and pushes, with a credential scoped to one attempt.
//!
//! Slots of one node share the repository. Fetch, worktree add/remove and gc
//! take the repository's lock; work inside one worktree needs none.
//! Every call blocks (it runs git); async callers use `spawn_blocking`.
pub mod transport;

use anyhow::{Context as _, Result, bail, ensure};
use fs2::FileExt as _;
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

/// The git part of an attempt's offer (aq-admission `startup::GitGrant`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub repository_id: Uuid,
    /// The commit a new task starts from.
    pub base: String,
    /// `refs/heads/aq/<task-id>`, the only ref the attempt pushes.
    pub task_ref: String,
    /// The task ref's head in central aq-git at offer time; `None`: no branch yet.
    pub lease: Option<String>,
    /// The followed default branch, fetched read-only.
    pub main_ref: String,
}

impl Grant {
    /// Every value later reaches git's argv: refuse anything that is not a
    /// full object id or a plain branch name.
    pub fn validate(&self) -> Result<()> {
        let oid = |v: &str| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit());
        let branch = |v: &str| {
            v.starts_with("refs/heads/")
                && !v.contains("..")
                && !v.ends_with('/')
                && !v.ends_with(".lock")
                && !v
                    .bytes()
                    .any(|b| b <= b' ' || b == 127 || b"~^:?*[\\@".contains(&b))
        };
        ensure!(
            oid(&self.base)
                && self.lease.as_deref().is_none_or(oid)
                && branch(&self.main_ref)
                && branch(&self.task_ref)
                && self.task_ref.starts_with("refs/heads/aq/"),
            "git grant shape"
        );
        Ok(())
    }
    fn tracking(r: &str) -> String {
        format!("{TRACKING}{}", r.trim_start_matches("refs/heads/"))
    }
}

/// How an attempt starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Start {
    /// A new task: from the grant's base.
    #[default]
    Base,
    /// A task a playbook returned (`work.return`): from its task branch head.
    TaskBranch,
}

/// Limits come from the node's configuration, never constants here.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Each git command is killed after this.
    pub command_timeout: Duration,
    /// Push attempts in total, all under the same lease.
    pub push_attempts: u32,
    pub retry_delay: Duration,
}

/// The node's git state: `root/repositories/<id>.git` per project and
/// `root/worktrees/<attempt>` per attempt.
pub struct NodeGit {
    git: PathBuf,
    root: PathBuf,
    /// Author and committer of the node's commits.
    identity: (String, String),
    limits: Limits,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// The node had no repository for the project and cloned it.
    Cloned,
    Fetched,
}

/// One attempt's worktree. Removed by [`NodeGit::end`].
#[derive(Debug)]
pub struct Worktree {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub grant: Grant,
    /// The commit the attempt started from.
    pub start: String,
    pub synced: Synced,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pushed {
    /// The task ref is at `commit` in central aq-git.
    Pushed { task_ref: String, commit: String },
    /// The task ref is no longer at the lease: report `push_conflict`.
    Conflict,
}

struct Output {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl NodeGit {
    pub fn new(git: PathBuf, root: PathBuf, identity: (String, String), limits: Limits) -> Self {
        Self {
            git,
            root,
            identity,
            limits,
        }
    }

    pub fn repository(&self, id: Uuid) -> PathBuf {
        self.root.join("repositories").join(format!("{id}.git"))
    }

    pub fn worktree_path(&self, attempt: Uuid) -> PathBuf {
        self.root.join("worktrees").join(attempt.to_string())
    }

    /// The project's repository is current: cloned the first time, fetched after.
    /// Fetches the main ref and, when the task branch exists, the task ref.
    fn sync(&self, remote: &Remote, grant: &Grant, repo: &Path) -> Result<Synced> {
        let synced = if repo.join("HEAD").exists() {
            Synced::Fetched
        } else {
            // A clone cut short leaves no HEAD and is started again.
            let _ = std::fs::remove_dir_all(repo);
            std::fs::create_dir_all(repo)?;
            self.ok(repo, &["init", "--bare", "--quiet"], None)?;
            Synced::Cloned
        };
        let url = remote.url(grant.repository_id);
        let main = format!("+{}:{}", grant.main_ref, Grant::tracking(&grant.main_ref));
        let task = format!("+{}:{}", grant.task_ref, Grant::tracking(&grant.task_ref));
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &url,
            &main,
        ];
        if grant.lease.is_some() {
            args.push(&task);
        }
        self.ok(repo, &args, Some(remote))?;
        for commit in std::iter::once(&grant.base).chain(&grant.lease) {
            self.ok(
                repo,
                &["cat-file", "-e", &format!("{commit}^{{commit}}")],
                None,
            )
            .context("the grant's commit is not in central aq-git")?;
        }
        Ok(synced)
    }

    /// Syncs the project's repository and adds the attempt's worktree on the
    /// task branch, from the base or (a returned task) the task branch head.
    pub fn start(
        &self,
        remote: &Remote,
        grant: &Grant,
        attempt: Uuid,
        start: Start,
    ) -> Result<Worktree> {
        grant.validate()?;
        let repo = self.repository(grant.repository_id);
        let path = self.worktree_path(attempt);
        let _lock = self.lock(grant.repository_id)?;
        let synced = self.sync(remote, grant, &repo)?;
        let from = match start {
            Start::Base => grant.base.clone(),
            Start::TaskBranch => grant
                .lease
                .clone()
                .context("a returned task has no task branch")?,
        };
        // A worktree an earlier boot left behind for this attempt is replaced.
        self.remove_worktree(&repo, &path)?;
        std::fs::create_dir_all(path.parent().context("worktree parent")?)?;
        let branch = grant.task_ref.trim_start_matches("refs/heads/");
        let target = path.to_str().context("worktree path UTF-8")?;
        self.ok(
            &repo,
            &[
                "worktree", "add", "--quiet", "--force", "-B", branch, target, &from,
            ],
            None,
        )?;
        Ok(Worktree {
            repository: repo,
            path,
            grant: grant.clone(),
            start: from,
            synced,
        })
    }

    /// After the stopped-writer proof and the copy-back (whose secret scan runs
    /// before this): commits the whole tree, as `workspace.checkpoint mode:all`
    /// did, and pushes it to the task ref under the grant's lease. A lost or
    /// refused push is retried with the same lease; a lease mismatch is
    /// [`Pushed::Conflict`]. Never forces.
    pub fn finish(&self, remote: &Remote, worktree: &Worktree, message: &str) -> Result<Pushed> {
        let dir = &worktree.path;
        self.ok(dir, &["add", "--all"], None)?;
        if !self.run(dir, &["diff", "--cached", "--quiet"], None)?.ok {
            self.ok(
                dir,
                &["commit", "--quiet", "--no-verify", "-m", message],
                None,
            )?;
        }
        let commit = self.ok(dir, &["rev-parse", "HEAD"], None)?;
        let grant = &worktree.grant;
        let lease = format!(
            "--force-with-lease={}:{}",
            grant.task_ref,
            grant.lease.as_deref().unwrap_or_default()
        );
        let url = remote.url(grant.repository_id);
        let refspec = format!("{commit}:{}", grant.task_ref);
        let args = ["push", "--porcelain", "--no-verify", &lease, &url, &refspec];
        let mut last = String::new();
        for n in 0..self.limits.push_attempts {
            if n > 0 {
                std::thread::sleep(self.limits.retry_delay);
            }
            let out = self.run(dir, &args, Some(remote))?;
            // Porcelain: `<flag>\t<from>:<to>\t<summary>`; `=` is "up to date",
            // which is what a retry sees after a push whose reply was lost.
            let line = out.stdout.lines().find(|l| {
                l.split('\t')
                    .nth(1)
                    .is_some_and(|r| r.ends_with(&grant.task_ref))
            });
            match line {
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

    /// Removes the attempt's worktree, its local branch and fetched task ref,
    /// then lets git pack and prune the shared repository.
    pub fn end(&self, worktree: &Worktree) -> Result<()> {
        let _lock = self.lock(worktree.grant.repository_id)?;
        let repo = &worktree.repository;
        self.remove_worktree(repo, &worktree.path)?;
        let branch = &worktree.grant.task_ref;
        for r in [branch.clone(), Grant::tracking(branch)] {
            self.ok(repo, &["update-ref", "-d", &r], None)?;
        }
        self.ok(repo, &["gc", "--auto", "--quiet"], None)?;
        Ok(())
    }

    fn remove_worktree(&self, repo: &Path, path: &Path) -> Result<()> {
        if path.exists() {
            let target = path.to_str().context("worktree path UTF-8")?;
            let out = self.run(
                repo,
                &["worktree", "remove", "--force", "--force", target],
                None,
            )?;
            if !out.ok {
                std::fs::remove_dir_all(path)?;
            }
        }
        self.ok(repo, &["worktree", "prune"], None)?;
        Ok(())
    }

    fn lock(&self, id: Uuid) -> Result<std::fs::File> {
        let dir = self.root.join("repositories");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::File::create(dir.join(format!("{id}.lock")))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn ok(&self, dir: &Path, args: &[&str], remote: Option<&Remote>) -> Result<String> {
        let out = self.run(dir, args, remote)?;
        if !out.ok {
            let said = format!("{}{}", out.stdout, out.stderr);
            let said = remote.map_or(said.clone(), |r| r.redact(&said));
            bail!("git {} failed ({:?}): {}", args[0], out.code, said.trim());
        }
        Ok(out.stdout.trim().to_owned())
    }

    /// Runs the node's git with no inherited configuration, prompts or hooks,
    /// killed with its process group after the command timeout.
    fn run(&self, dir: &Path, args: &[&str], remote: Option<&Remote>) -> Result<Output> {
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
        command
            .args(args)
            .env_clear()
            .env("LC_ALL", "C")
            .env("HOME", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(remote) = remote {
            remote.apply(&mut command);
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

#[cfg(test)]
mod tests;
