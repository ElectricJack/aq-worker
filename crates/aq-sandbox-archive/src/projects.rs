//! The sandbox receives source trees. Git and its authority stay on the node.
use crate::{
    docker_engine::WorkspaceBindPaths,
    tar::TreeEntry,
    workspace::{collect_tree, sync_tree},
};
use anyhow::{Context, Result};
use aq_node_git::workspaces::{self, Access};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn stage_readonly(
    primary: &Path,
    limit: u64,
    mapping: Option<&WorkspaceBindPaths>,
    mounts: &mut BTreeMap<uuid::Uuid, PathBuf>,
) -> Result<u64> {
    let plan = workspaces::load(primary)?;
    let mut remaining = limit;
    for entry in plan
        .workspaces
        .into_iter()
        .filter(|entry| entry.access == Access::Read)
    {
        let checkout = workspaces::checkout_path(primary, entry.project_id);
        let snapshot = workspaces::readonly_path(primary, entry.project_id);
        let entries = collect_tree(&checkout, remaining)?;
        remaining = after_tree(remaining, &entries)?;
        std::fs::create_dir_all(&snapshot)?;
        sync_tree(&snapshot, &entries)?;
        let target =
            mapping.context("read-only projects require a worker-to-daemon path mapping")?;
        mounts.insert(entry.project_id, target.on_daemon(&snapshot)?);
    }
    Ok(remaining)
}

pub struct ProjectTree {
    pub project: uuid::Uuid,
    pub path: PathBuf,
    pub entries: Vec<TreeEntry>,
}

pub fn after_tree(mut remaining: u64, entries: &[TreeEntry]) -> Result<u64> {
    for entry in entries {
        if let TreeEntry::File { data, .. } = entry {
            remaining = remaining
                .checked_sub(data.len() as u64)
                .context("sibling workspace byte bound")?;
        }
    }
    Ok(remaining)
}

pub fn writable_trees(primary: &Path, limit: u64) -> Result<Vec<ProjectTree>> {
    let plan = workspaces::load(primary)?;
    let mut remaining = limit;
    let mut trees = Vec::new();
    for entry in plan
        .workspaces
        .into_iter()
        .filter(|entry| entry.access == Access::Write)
    {
        let path = workspaces::checkout_path(primary, entry.project_id);
        let entries = collect_tree(&path, remaining)?;
        remaining = after_tree(remaining, &entries)?;
        trees.push(ProjectTree {
            project: entry.project_id,
            path,
            entries,
        });
    }
    Ok(trees)
}

pub async fn returned_trees<F>(
    primary: &Path,
    mut remaining: u64,
    mut read: impl FnMut(uuid::Uuid, u64) -> F,
) -> Result<Vec<ProjectTree>>
where
    F: Future<Output = Result<Vec<TreeEntry>>> + Send,
{
    let plan = workspaces::load(primary)?;
    let mut trees = Vec::new();
    for entry in plan
        .workspaces
        .into_iter()
        .filter(|entry| entry.access == Access::Write)
    {
        let project = entry.project_id;
        let entries = read(project, remaining)
            .await
            .with_context(|| format!("copy-back of project {project}"))?;
        remaining = after_tree(remaining, &entries)?;
        trees.push(ProjectTree {
            project,
            path: workspaces::checkout_path(primary, project),
            entries,
        });
    }
    Ok(trees)
}

pub fn apply(trees: &[ProjectTree]) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for tree in trees {
        changed.extend(
            sync_tree(&tree.path, &tree.entries)?
                .into_iter()
                .map(|path| format!("{}/{path}", workspaces::sandbox_path(tree.project))),
        );
    }
    Ok(changed)
}

pub fn note(primary: &Path) -> Result<String> {
    let mut note = String::new();
    for entry in workspaces::load(primary)?.workspaces {
        use std::fmt::Write as _;
        let access = if entry.access == Access::Read {
            "read-only"
        } else {
            "writable"
        };
        writeln!(
            note,
            "Related project {}: {} ({access}, revision {}).",
            entry.project_id,
            workspaces::sandbox_path(entry.project_id),
            entry.checkout_head,
        )?;
    }
    if !note.is_empty() {
        note.insert_str(0, "\nSibling checkouts are mounted separately from Home (/aq/work). Run each project's checks in its own checkout.\n");
    }
    Ok(note)
}

/// Verify every recovered tree before a checkout is modified.
pub fn refuse_credentials(primary: &[TreeEntry], siblings: &[ProjectTree]) -> Result<()> {
    use crate::workspace::credential_shaped;
    anyhow::ensure!(
        !credential_shaped(primary)
            && siblings
                .iter()
                .all(|tree| !credential_shaped(&tree.entries)),
        "credential in stopped workspace",
    );
    Ok(())
}
