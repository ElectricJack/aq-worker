//! Final-source snapshot of a stopped workspace, including the git directory,
//! dirty tracked files and untracked files. This is the worker-side snapshot
//! format until aq-git publishes a dirty/untracked snapshot operation; the
//! suspender only consumes it through `SourceArchive::capture`/`restore`.
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

pub const FORMAT: &str = "aq-hibernation-source-v1";
/// Snapshots larger than this are an explicit hold, never a silent drop.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub mode: u32,
    pub data: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceArchive {
    pub format: String,
    pub entries: Vec<Entry>,
}

/// Why a snapshot cannot be taken: always reported as a hold.
#[derive(Debug)]
pub struct Unsupported(pub &'static str);

impl SourceArchive {
    pub fn capture(root: &Path) -> Result<std::result::Result<Self, Unsupported>> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut children: Vec<_> = std::fs::read_dir(&dir)
                .with_context(|| format!("read {}", dir.display()))?
                .collect::<std::io::Result<_>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let path = child.path();
                let kind = child.file_type()?;
                if kind.is_symlink() {
                    return Ok(Err(Unsupported("unsupported snapshot content: symlink")));
                } else if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file() {
                    let meta = child.metadata()?;
                    total += meta.len();
                    if total > MAX_SOURCE_BYTES {
                        return Ok(Err(Unsupported("source snapshot exceeds size bound")));
                    }
                    let rel = path
                        .strip_prefix(root)?
                        .to_str()
                        .context("non-UTF-8 path")?;
                    entries.push(Entry {
                        path: rel.to_owned(),
                        mode: meta.permissions().mode() & 0o777,
                        data: base64::engine::general_purpose::STANDARD
                            .encode(std::fs::read(&path)?),
                    });
                } else {
                    return Ok(Err(Unsupported(
                        "unsupported snapshot content: special file",
                    )));
                }
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Ok(Self {
            format: FORMAT.to_owned(),
            entries,
        }))
    }

    pub fn decoded(&self) -> Result<Vec<(&str, Vec<u8>)>> {
        self.entries
            .iter()
            .map(|e| {
                Ok((
                    e.path.as_str(),
                    base64::engine::general_purpose::STANDARD.decode(&e.data)?,
                ))
            })
            .collect()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes).context("source archive")?;
        if value.format != FORMAT {
            bail!("unsupported source archive format");
        }
        Ok(value)
    }

    /// Restore into an empty (or absent) directory of a fresh sandbox.
    pub fn restore(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(root)?;
        if std::fs::read_dir(root)?.next().is_some() {
            bail!("continuation workspace is not empty");
        }
        for (path, bytes) in self.decoded()? {
            let rel = Path::new(path);
            if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
                bail!("source archive path escapes workspace");
            }
            let target = root.join(rel);
            std::fs::create_dir_all(target.parent().context("entry parent")?)?;
            std::fs::write(&target, bytes)?;
            let mode = self
                .entries
                .iter()
                .find(|e| e.path == path)
                .map_or(0o644, |e| e.mode);
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_round_trips_and_refuses_escape_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("a/b/f.txt"), b"hello").unwrap();
        std::fs::write(src.join("run.sh"), b"#!/bin/sh").unwrap();
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let archive = SourceArchive::capture(&src).unwrap().unwrap();
        let back = SourceArchive::from_bytes(&archive.to_bytes().unwrap()).unwrap();
        let dst = dir.path().join("dst");
        back.restore(&dst).unwrap();
        assert_eq!(std::fs::read(dst.join("a/b/f.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::metadata(dst.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let mut evil = back.clone();
        evil.entries[0].path = "../x".into();
        assert!(evil.restore(&dir.path().join("evil")).is_err());
        std::os::unix::fs::symlink("/etc/passwd", src.join("link")).unwrap();
        assert!(SourceArchive::capture(&src).unwrap().is_err());
    }
}
