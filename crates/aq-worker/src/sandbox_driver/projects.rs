//! The sandbox receives source trees. Git and its authority stay on the node.
use super::*;
use aq_node_git::workspaces::{self, Access};

pub(super) fn stage_readonly(
    primary: &Path,
    limit: u64,
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
        mounts.insert(entry.project_id, snapshot);
    }
    Ok(remaining)
}

pub(crate) struct ProjectTree {
    pub(crate) project: uuid::Uuid,
    pub(crate) path: PathBuf,
    pub(crate) entries: Vec<TreeEntry>,
}

pub(crate) fn after_tree(mut remaining: u64, entries: &[TreeEntry]) -> Result<u64> {
    for entry in entries {
        if let TreeEntry::File { data, .. } = entry {
            remaining = remaining
                .checked_sub(data.len() as u64)
                .context("sibling workspace byte bound")?;
        }
    }
    Ok(remaining)
}

pub(super) fn writable_trees(primary: &Path, limit: u64) -> Result<Vec<ProjectTree>> {
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

pub(crate) async fn returned_trees<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
    primary: &Path,
    mut remaining: u64,
) -> Result<Vec<ProjectTree>> {
    let plan = workspaces::load(primary)?;
    let mut trees = Vec::new();
    for entry in plan
        .workspaces
        .into_iter()
        .filter(|entry| entry.access == Access::Write)
    {
        let project = entry.project_id;
        let entries = backend
            .stopped_project_tree(sandbox, project, remaining)
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

pub(crate) fn apply(trees: &[ProjectTree]) -> Result<Vec<String>> {
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

pub(super) fn note(primary: &Path) -> Result<String> {
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
            entry.git.base,
        )?;
    }
    if !note.is_empty() {
        note.insert_str(0, "\n");
    }
    Ok(note)
}
