//! Minimal ustar writer/reader for the Docker archive API. Material archives
//! are built in one pre-sized zeroizing buffer, so no reallocation leaves a copy.
use zeroize::Zeroizing;
mod reader;
pub use reader::{read_named_tree, read_tree, read_workspace_tree, single_file};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidArchive(pub &'static str);

const BLOCK: usize = 512;

fn padded(len: usize) -> usize {
    len.div_ceil(BLOCK) * BLOCK
}

struct Member<'a> {
    mode: u32,
    size: usize,
    kind: u8,
    link: &'a str,
    mtime: u64,
}

impl<'a> Member<'a> {
    fn file(mode: u32, size: usize, mtime: u64) -> Self {
        Self {
            mode,
            size,
            mtime,
            kind: b'0',
            link: "",
        }
    }

    fn directory(mode: u32) -> Self {
        Self {
            kind: b'5',
            ..Self::file(mode, 0, 0)
        }
    }

    fn link(mode: u32, kind: u8, target: &'a str) -> Self {
        Self {
            kind,
            link: target,
            ..Self::file(mode, 0, 0)
        }
    }
}

fn header(name: &str, owner: (u32, u32), member: &Member<'_>) -> [u8; BLOCK] {
    let mut header = [0u8; BLOCK];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let fields: [(usize, usize, u64); 5] = [
        (100, 8, u64::from(member.mode)),
        (108, 8, u64::from(owner.0)),
        (116, 8, u64::from(owner.1)),
        (124, 12, member.size as u64),
        (136, 12, member.mtime),
    ];
    for (start, len, value) in fields {
        let text = format!("{value:0width$o}\0", width = len - 1);
        header[start..start + len].copy_from_slice(text.as_bytes());
    }
    header[156] = member.kind;
    let link = member.link.as_bytes();
    let length = link.len().min(100);
    header[157..157 + length].copy_from_slice(&link[..length]);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}

/// Archive for upload at `/`: intermediate directories 0755 root, the material's
/// own directory 0700 and the file 0600, both owned by `uid:gid`.
pub fn material_archive(
    path: &str,
    data: &[u8],
    uid: u32,
    gid: u32,
) -> Result<Zeroizing<Vec<u8>>, InvalidArchive> {
    let relative = path
        .strip_prefix('/')
        .filter(|rest| rest.len() < 100 && rest.contains('/'))
        .ok_or(InvalidArchive("material path unsupported by archive"))?;
    let parts: Vec<&str> = relative.split('/').collect();
    let dirs = parts.len() - 1;
    let total = BLOCK * (dirs + 1) + padded(data.len()) + 2 * BLOCK;
    let mut archive = Zeroizing::new(Vec::with_capacity(total));
    for depth in 1..=dirs {
        let name = format!("{}/", parts[..depth].join("/"));
        let (mode, owner) = if depth == dirs {
            (0o700, (uid, gid))
        } else {
            (0o755, (0, 0))
        };
        archive.extend_from_slice(&header(&name, owner, &Member::directory(mode)));
    }
    archive.extend_from_slice(&header(
        relative,
        (uid, gid),
        &Member::file(0o600, data.len(), 0),
    ));
    archive.extend_from_slice(data);
    archive.resize(total, 0);
    debug_assert_eq!(archive.capacity(), total);
    Ok(archive)
}

/// One member of a non-secret tree archive. Paths are relative and normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntry {
    Dir {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        data: Vec<u8>,
    },
    Symlink {
        path: String,
        target: String,
    },
}
impl TreeEntry {
    pub fn path(&self) -> &str {
        match self {
            Self::Dir { path, .. } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

fn push_member(out: &mut Vec<u8>, name: &str, member: Member<'_>) {
    if name.len() >= 100 {
        // GNU long name record, then a truncated name in the real header.
        let mut long = name.as_bytes().to_vec();
        long.push(0);
        let metadata = Member {
            kind: b'L',
            ..Member::file(0o644, long.len(), 0)
        };
        out.extend_from_slice(&header("././@LongLink", (0, 0), &metadata));
        let start = out.len();
        out.extend_from_slice(&long);
        out.resize(start + padded(long.len()), 0);
    }
    let short: String = name.chars().take(99).collect();
    out.extend_from_slice(&header(&short, (0, 0), &member));
}

/// Archive for upload at `/`: every entry is placed under `prefix` (relative,
/// e.g. `aq/work`). Owner is root (the sandbox runner in single-UID mode).
pub fn tree_archive(prefix: &str, entries: &[TreeEntry]) -> Result<Vec<u8>, InvalidArchive> {
    tree_archive_at(prefix, entries, &|_| 0)
}

/// `tree_archive` with a modification time (Unix seconds) per file path, for
/// build tools that judge freshness by mtime (`build_cache`).
pub fn tree_archive_at(
    prefix: &str,
    entries: &[TreeEntry],
    mtime: &dyn Fn(&str) -> u64,
) -> Result<Vec<u8>, InvalidArchive> {
    let bad = InvalidArchive("tree archive path unsupported");
    if prefix
        .split('/')
        .any(|component| matches!(component, "" | ".."))
    {
        return Err(bad);
    }
    let mut out = Vec::new();
    let mut acc = String::new();
    for part in prefix.split('/') {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        push_member(&mut out, &format!("{acc}/"), Member::directory(0o755));
    }
    for entry in entries {
        write_tree_entry(&mut out, prefix, entry, mtime)?;
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

fn write_tree_entry(
    out: &mut Vec<u8>,
    prefix: &str,
    entry: &TreeEntry,
    mtime: &dyn Fn(&str) -> u64,
) -> Result<(), InvalidArchive> {
    let bad = InvalidArchive("tree archive path unsupported");
    let rel = entry.path();
    if !valid_relative(rel) {
        return Err(bad);
    }
    let name = format!("{prefix}/{rel}");
    match entry {
        TreeEntry::Dir { mode, .. } => {
            push_member(out, &format!("{name}/"), Member::directory(*mode))
        }
        TreeEntry::File { mode, data, .. } => {
            push_member(out, &name, Member::file(*mode, data.len(), mtime(rel)));
            let start = out.len();
            out.extend_from_slice(data);
            out.resize(start + padded(data.len()), 0);
        }
        TreeEntry::Symlink { target, .. } => {
            if target.len() >= 100 {
                return Err(bad);
            }
            push_member(out, &name, Member::link(0o777, b'2', target))
        }
    }
    Ok(())
}

fn valid_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

#[cfg(test)]
mod tests {
    use super::reader::octal;
    use super::*;

    #[test]
    fn material_archive_round_trips_with_private_modes() {
        let archive = material_archive("/aq/private/material", b"secret", 1000, 1000).unwrap();
        assert_eq!(archive.len() % BLOCK, 0);
        assert_eq!(&archive[..3], b"aq/");
        assert_eq!(octal(&archive[100..108]), Some(0o755));
        assert_eq!(&archive[BLOCK..BLOCK + 11], b"aq/private/");
        assert_eq!(octal(&archive[BLOCK + 100..BLOCK + 108]), Some(0o700));
        let file = &archive[2 * BLOCK..];
        assert_eq!(octal(&file[100..108]), Some(0o600));
        assert_eq!(octal(&file[108..116]), Some(1000));
        assert_eq!(single_file(file, 6).unwrap(), b"secret");
        assert_eq!(
            single_file(file, 5),
            Err(InvalidArchive("archive exceeds read cap"))
        );
    }

    #[test]
    fn single_file_rejects_corrupt_header() {
        let archive = material_archive("/aq/x/m", b"data", 0, 0).unwrap();
        let mut file = archive[2 * BLOCK..].to_vec();
        file[0] ^= 1;
        assert!(single_file(&file, 100).is_err());
        assert!(single_file(&archive, 100).is_err(), "directory first");
    }
    #[test]
    fn volume_archive_preserves_root_and_nested_files_and_rejects_partial_data() {
        let files = [
            TreeEntry::File {
                path: "one".into(),
                mode: 0o644,
                data: b"first".to_vec(),
            },
            TreeEntry::File {
                path: "dir/two".into(),
                mode: 0o644,
                data: b"second".to_vec(),
            },
        ];
        let archive = tree_archive(".", &files).unwrap();
        let entries = read_workspace_tree(&archive, 1024, false).unwrap();
        assert_eq!(
            entries.iter().map(TreeEntry::path).collect::<Vec<_>>(),
            ["one", "dir/two"]
        );
        assert!(read_workspace_tree(&archive[..archive.len() - BLOCK], 1024, false).is_err());
        assert!(read_workspace_tree(&archive, 5, false).is_err());
    }

    fn sample_tree(long: &str) -> Vec<TreeEntry> {
        vec![
            TreeEntry::Dir {
                path: "src".into(),
                mode: 0o755,
            },
            TreeEntry::File {
                path: "src/lib.rs".into(),
                mode: 0o644,
                data: b"fn x() {}\n".to_vec(),
            },
            TreeEntry::File {
                path: long.to_owned(),
                mode: 0o755,
                data: vec![7; 700],
            },
            TreeEntry::Symlink {
                path: "link".into(),
                target: "src/lib.rs".into(),
            },
        ]
    }

    #[test]
    fn tree_archive_round_trips_long_names_and_refuses_traversal() {
        let long = format!("{}/file.txt", "d".repeat(120));
        let entries = sample_tree(&long);
        let archive = tree_archive("aq/work", &entries).unwrap();
        // A GET of /aq/work names members "work/...": strip one more level.
        let read = read_tree(&archive, 1 << 20).unwrap();
        let read: Vec<_> = read
            .into_iter()
            .filter_map(|e| match e {
                TreeEntry::Dir { path, mode } => {
                    path.strip_prefix("work/").map(|p| TreeEntry::Dir {
                        path: p.into(),
                        mode,
                    })
                }
                TreeEntry::File { path, mode, data } => {
                    path.strip_prefix("work/").map(|p| TreeEntry::File {
                        path: p.into(),
                        mode,
                        data,
                    })
                }
                TreeEntry::Symlink { path, target } => {
                    path.strip_prefix("work/").map(|p| TreeEntry::Symlink {
                        path: p.into(),
                        target,
                    })
                }
            })
            .collect();
        assert_eq!(read, entries);
        assert!(
            tree_archive(
                "aq/work",
                &[TreeEntry::Dir {
                    path: "../x".into(),
                    mode: 0o755
                }]
            )
            .is_err()
        );
        assert!(tree_archive("/aq", &entries).is_err());
        let mut evil = Vec::new();
        push_member(
            &mut evil,
            "work/../../etc/passwd",
            Member::file(0o644, 0, 0),
        );
        evil.resize(evil.len() + 2 * BLOCK, 0);
        assert!(read_tree(&evil, 1 << 20).is_err());
        assert_eq!(
            read_tree(&archive, 100),
            Err(InvalidArchive("archive exceeds read cap"))
        );
    }

    #[test]
    fn hard_links_become_copies_of_their_earlier_file() {
        let mut a = Vec::new();
        push_member(&mut a, "/", Member::directory(0o755));
        push_member(&mut a, "/f", Member::file(0o644, 3, 0));
        let start = a.len();
        a.extend_from_slice(b"abc");
        a.resize(start + padded(3), 0);
        push_member(&mut a, "/g", Member::link(0o644, b'1', "/f"));
        a.resize(a.len() + 2 * BLOCK, 0);
        let read = read_tree(&a, 1 << 20).unwrap();
        assert_eq!(
            read[1],
            TreeEntry::File {
                path: "g".into(),
                mode: 0o644,
                data: b"abc".to_vec()
            }
        );
        let mut bad = Vec::new();
        push_member(&mut bad, "/g", Member::link(0o644, b'1', "/missing"));
        bad.resize(bad.len() + 2 * BLOCK, 0);
        assert!(read_tree(&bad, 1 << 20).is_err());
    }

    #[test]
    fn a_tree_archive_carries_each_files_mtime_and_still_reads_back() {
        let file = |path: &str| TreeEntry::File {
            path: path.into(),
            mode: 0o644,
            data: b"x".to_vec(),
        };
        let entries = [file("old.rs"), file("new.rs")];
        let stamp = |path: &str| if path == "new.rs" { 1_800_000_000 } else { 0 };
        let archive = tree_archive_at("work", &entries, &stamp).unwrap();
        // One directory header, then one header and one data block per file.
        let mtime = |block: usize| octal(&archive[block * BLOCK + 136..block * BLOCK + 148]);
        assert_eq!((mtime(1), mtime(3)), (Some(0), Some(1_800_000_000)));
        assert_eq!(read_tree(&archive, 16).unwrap(), entries);
    }
}
