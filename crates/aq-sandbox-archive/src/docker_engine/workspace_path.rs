//! Translate a private source snapshot through an operator's state bind.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBindPaths {
    pub worker_root: PathBuf,
    pub daemon_root: PathBuf,
}

fn root(path: &Path) -> bool {
    let mut parts = path.components();
    parts.next() == Some(Component::RootDir)
        && matches!(parts.next(), Some(Component::Normal(_)))
        && parts.all(|part| matches!(part, Component::Normal(_)))
}

impl WorkspaceBindPaths {
    /// Managed worker packages bind the operator's state directory at /state.
    /// This setting is never obtained from the sandbox's own environment.
    pub fn managed() -> Option<Self> {
        let mapping = Self {
            worker_root: "/state".into(),
            daemon_root: std::env::var_os("AQ_WORKER_DAEMON_STATE_ROOT")?.into(),
        };
        (root(&mapping.daemon_root) && mapping.worker_root.is_dir()).then_some(mapping)
    }

    pub fn on_daemon(&self, snapshot: &Path) -> Result<PathBuf> {
        ensure!(
            root(&self.worker_root) && root(&self.daemon_root),
            "invalid project bind roots"
        );
        let local = self.worker_root.canonicalize()?;
        let path = snapshot.canonicalize()?;
        let relative = path
            .strip_prefix(local)
            .context("project snapshot is outside mapped worker state")?;
        ensure!(
            !relative.as_os_str().is_empty(),
            "cannot mount the worker state root"
        );
        Ok(self.daemon_root.join(relative))
    }
}
