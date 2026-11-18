//! The node driver's workspace copy: a worktree as archive entries (copy-in) and
//! applying the sandbox's entries back onto it (copy-out). Moved from
//! `aq-worker`'s `sandbox_driver`, unchanged.
use crate::tar::TreeEntry;
use anyhow::{Context, Result, bail, ensure};
use aq_handoff_note::{CopyBackIncomplete, Secrets};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
};

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
        if let TreeEntry::Symlink { target, .. } = entry {
            // A link may only point inside the workspace, relatively.
            let target = Path::new(target);
            ensure!(
                !target.is_absolute()
                    && target
                        .components()
                        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
                "copy-back symlink target must be relative and stay inside the workspace"
            );
        }
        wanted.insert(rel.to_owned());
    }
    // No entry may sit below a path the archive itself makes a symlink: a
    // later write would otherwise go through that link.
    let links: BTreeSet<&str> = entries
        .iter()
        .filter(|e| matches!(e, TreeEntry::Symlink { .. }))
        .map(|e| e.path())
        .collect();
    for entry in entries {
        let mut parent = Path::new(entry.path()).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            ensure!(
                !links.contains(p.to_str().unwrap_or_default()),
                "copy-back entry below a symlinked parent"
            );
            parent = p.parent();
        }
    }
    // From here the worktree changes: a failure leaves part of the tree behind.
    apply_tree(root, entries, &wanted).context(CopyBackIncomplete)
}
fn apply_tree(
    root: &Path,
    entries: &[TreeEntry],
    wanted: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let (mut changed, git) = (Vec::new(), |p: &str| p == ".git" || p.starts_with(".git/"));
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

/// An entry's path and its content or link target.
pub fn tree_bytes(e: &TreeEntry) -> [&[u8]; 2] {
    let held: &[u8] = match e {
        TreeEntry::File { data, .. } => data,
        TreeEntry::Symlink { target, .. } => target.as_bytes(),
        TreeEntry::Dir { .. } => b"",
    };
    [e.path().as_bytes(), held]
}

/// MS1 review S3b: the agent's tools can read the token from the claude environment. Refuse the
/// copy-back when a copied-back entry or the transcript holds a secret the base tree did not.
pub fn token_leak(entries: &[TreeEntry], stream: &[u8], by: &Secrets) -> Option<&'static str> {
    if entries.iter().flat_map(tree_bytes).any(|b| by.holds(b)) {
        Some("a secret (login token or AQ credential) appeared in the workspace")
    } else if by.holds(stream) {
        Some("a secret (login token or AQ credential) appeared in the Claude transcript")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_handoff_note::{preserved_claim, print_logs};

    #[test]
    fn copy_back_refuses_symlinked_parents_and_escaping_links() {
        let root = tempfile::tempdir().unwrap();
        let link = |path: &str, target: &str| TreeEntry::Symlink {
            path: path.into(),
            target: target.into(),
        };
        let file = |path: &str| TreeEntry::File {
            path: path.into(),
            mode: 0o644,
            data: b"x".to_vec(),
        };
        for bad in [
            vec![link("out", "/etc")],
            vec![link("out", "../outside")],
            vec![link("out", "sub/../../x")],
            vec![link("d", "sub"), file("d/escape.txt")],
        ] {
            assert!(sync_tree(root.path(), &bad).is_err());
        }
        assert!(!root.path().join("d").exists() && !root.path().join("out").exists());
        sync_tree(
            root.path(),
            &[file("a.txt"), link("b", "a.txt"), link("c", "./a.txt")],
        )
        .unwrap();
        assert!(root.path().join("b").is_symlink());
    }

    /// Review #8: a copy-back that fails after it began writing is marked incomplete and
    /// claims nothing; a refusal before any write still claims the preserved sandbox.
    #[test]
    fn partial_copy_back_is_never_claimed() {
        let root = tempfile::tempdir().unwrap();
        let file = |path: &str| TreeEntry::File {
            path: path.into(),
            mode: 0o644,
            data: b"x".to_vec(),
        };
        let partial = sync_tree(root.path(), &[file("a.txt"), file("missing/b.txt")]).unwrap_err();
        assert!(
            root.path().join("a.txt").exists(),
            "a.txt was already written"
        );
        let config = tempfile::tempdir().unwrap();
        let config = config.path().join("driver.json");
        assert_eq!(preserved_claim(&partial, "sb", &config), None);
        let link = TreeEntry::Symlink {
            path: "out".into(),
            target: "/etc".into(),
        };
        let refused = sync_tree(root.path(), &[link]).unwrap_err();
        assert!(
            preserved_claim(&refused, "sb", &config)
                .unwrap()
                .starts_with("Sandbox sb preserved;")
        );
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
        let by = &Secrets::new(&token, None);
        assert_eq!(token_leak(&clean, b"{\"type\":\"result\"}", by), None);
        let mut leaked = token.clone();
        leaked.splice(0..0, b"export TOKEN=".iter().copied());
        assert!(token_leak(&[file("notes.txt", &leaked)], b"", by).is_some());
        let name = format!("x{}", String::from_utf8(token.clone()).unwrap());
        assert!(token_leak(&[file(&name, b"ok")], b"", by).is_some());
        let link = TreeEntry::Symlink {
            path: "l".into(),
            target: String::from_utf8(token.clone()).unwrap(),
        };
        assert!(token_leak(&[link], b"", by).is_some());
        assert!(token_leak(&clean, &leaked, by).is_some_and(|w| w.contains("transcript")));
        // A near miss is not the token.
        assert_eq!(token_leak(&clean, &token[..token.len() - 1], by), None);
        let mut partial = Vec::new();
        let mut lines = b"fine\n".to_vec();
        lines.extend_from_slice(&leaked);
        lines.push(b'\n');
        assert!(print_logs(&lines, &mut partial, by));
        assert!(!print_logs(b"also fine\n", &mut partial, by));
    }
}
