//! Minimal ustar writer/reader for the Docker archive API. Material archives
//! are built in one pre-sized zeroizing buffer, so no reallocation leaves a copy.
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidArchive(pub &'static str);

const BLOCK: usize = 512;

fn padded(len: usize) -> usize {
    len.div_ceil(BLOCK) * BLOCK
}

fn header(name: &str, mode: u32, uid: u32, gid: u32, size: usize, kind: u8) -> [u8; BLOCK] {
    let mut header = [0u8; BLOCK];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let fields: [(usize, usize, u64); 5] = [
        (100, 8, u64::from(mode)),
        (108, 8, u64::from(uid)),
        (116, 8, u64::from(gid)),
        (124, 12, size as u64),
        (136, 12, 0),
    ];
    for (start, len, value) in fields {
        let text = format!("{value:0width$o}\0", width = len - 1);
        header[start..start + len].copy_from_slice(text.as_bytes());
    }
    header[156] = kind;
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
        archive.extend_from_slice(&header(&name, mode, owner.0, owner.1, 0, b'5'));
    }
    archive.extend_from_slice(&header(relative, 0o600, uid, gid, data.len(), b'0'));
    archive.extend_from_slice(data);
    archive.resize(total, 0);
    debug_assert_eq!(archive.capacity(), total);
    Ok(archive)
}

fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

/// Returns the content of the archive's single regular file, skipping PAX and
/// GNU long-name records. Fails closed above `max` bytes or on a bad header.
pub fn single_file(archive: &[u8], max: u64) -> Result<Vec<u8>, InvalidArchive> {
    let malformed = InvalidArchive("archive is not a single regular file");
    let mut offset = 0;
    while let Some(header) = archive.get(offset..offset + BLOCK) {
        let stored = octal(&header[148..156]).ok_or(malformed)?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*b)
                }
            })
            .sum();
        if header.iter().all(|b| *b == 0) || sum != stored {
            return Err(malformed);
        }
        let size = octal(&header[124..136]).ok_or(malformed)?;
        match header[156] {
            b'0' | 0 => {
                if size > max {
                    return Err(InvalidArchive("archive exceeds read cap"));
                }
                let start = offset + BLOCK;
                return archive
                    .get(start..start + size as usize)
                    .map(<[u8]>::to_vec)
                    .ok_or(malformed);
            }
            b'x' | b'g' | b'L' | b'K' if size <= 1024 * 1024 => {
                offset += BLOCK + padded(size as usize);
            }
            _ => return Err(malformed),
        }
    }
    Err(malformed)
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

fn push_member(out: &mut Vec<u8>, name: &str, mode: u32, size: usize, kind: u8, link: &str) {
    push_member_at(out, name, mode, size, kind, link, 0)
}

#[allow(clippy::too_many_arguments)]
fn push_member_at(
    out: &mut Vec<u8>,
    name: &str,
    mode: u32,
    size: usize,
    kind: u8,
    link: &str,
    mtime: u64,
) {
    if name.len() >= 100 {
        // GNU long name record, then a truncated name in the real header.
        let mut long = name.as_bytes().to_vec();
        long.push(0);
        out.extend_from_slice(&header("././@LongLink", 0o644, 0, 0, long.len(), b'L'));
        let start = out.len();
        out.extend_from_slice(&long);
        out.resize(start + padded(long.len()), 0);
    }
    let short: String = name.chars().take(99).collect();
    let mut h = header(&short, mode, 0, 0, size, kind);
    if !link.is_empty() || mtime != 0 {
        h[157..157 + link.len().min(100)].copy_from_slice(&link.as_bytes()[..link.len().min(100)]);
        h[136..148].copy_from_slice(format!("{mtime:011o}\0").as_bytes());
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }
    out.extend_from_slice(&h);
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
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix.split('/').any(|c| c.is_empty() || c == "..")
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
        push_member(&mut out, &format!("{acc}/"), 0o755, 0, b'5', "");
    }
    for entry in entries {
        let rel = entry.path();
        if !valid_relative(rel) {
            return Err(bad);
        }
        let name = format!("{prefix}/{rel}");
        match entry {
            TreeEntry::Dir { mode, .. } => {
                push_member(&mut out, &format!("{name}/"), *mode, 0, b'5', "")
            }
            TreeEntry::File { mode, data, .. } => {
                push_member_at(&mut out, &name, *mode, data.len(), b'0', "", mtime(rel));
                let start = out.len();
                out.extend_from_slice(data);
                out.resize(start + padded(data.len()), 0);
            }
            TreeEntry::Symlink { target, .. } => {
                if target.len() >= 100 {
                    return Err(bad);
                }
                push_member(&mut out, &name, 0o777, 0, b'2', target)
            }
        }
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

fn valid_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn pax_path(record: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(record).ok()?;
    let mut rest = text;
    let mut found = None;
    while !rest.is_empty() {
        let (len, _) = rest.split_once(' ')?;
        let n: usize = len.parse().ok()?;
        let line = rest.get(..n)?;
        rest = &rest[n..];
        let (_, kv) = line.split_once(' ')?;
        let kv = kv.strip_suffix('\n')?;
        if let Some(v) = kv.strip_prefix("path=") {
            found = Some(v.to_owned());
        }
    }
    found
}

/// Reads a Docker `GET archive` of a directory. The leading component (the
/// directory's own name) is stripped. Unsafe or unsupported members are refused,
/// never skipped silently; hard links, devices and FIFOs are refused.
pub fn read_tree(archive: &[u8], max_total: u64) -> Result<Vec<TreeEntry>, InvalidArchive> {
    read_workspace_tree(archive, max_total, true)
}

/// Container archives carry a directory prefix. Volume exports have root-relative paths.
pub fn read_workspace_tree(
    archive: &[u8],
    max_total: u64,
    container_root: bool,
) -> Result<Vec<TreeEntry>, InvalidArchive> {
    let malformed = InvalidArchive("tree archive malformed or unsafe");
    let mut offset = 0;
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut long_name: Option<String> = None;
    let mut complete = false;
    while let Some(h) = archive.get(offset..offset + BLOCK) {
        if h.iter().all(|b| *b == 0) {
            complete = archive
                .get(offset + BLOCK..offset + 2 * BLOCK)
                .is_some_and(|next| next.iter().all(|b| *b == 0));
            break;
        }
        let stored = octal(&h[148..156]).ok_or(malformed)?;
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*b)
                }
            })
            .sum();
        if sum != stored {
            return Err(malformed);
        }
        let size = octal(&h[124..136]).ok_or(malformed)? as usize;
        let data = archive
            .get(offset + BLOCK..offset + BLOCK + size)
            .ok_or(malformed)?;
        offset += BLOCK + padded(size);
        let kind = h[156];
        if matches!(kind, b'L' | b'x') {
            if size > 64 * 1024 {
                return Err(malformed);
            }
            long_name = if kind == b'L' {
                Some(
                    String::from_utf8_lossy(data)
                        .trim_end_matches('\0')
                        .to_owned(),
                )
            } else {
                pax_path(data).or(long_name)
            };
            continue;
        }
        if kind == b'g' {
            continue;
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &h[range];
            let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let name = match long_name.take() {
            Some(name) => name,
            None => {
                let prefix = field(345..500);
                let name = field(0..100);
                if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                }
            }
        };
        let name = name.trim_start_matches("./").trim_end_matches('/');
        let rel = if container_root {
            let Some((_, rel)) = name.split_once('/') else {
                continue; // the archived directory itself
            };
            rel
        } else {
            if (name == "." || name.is_empty()) && kind == b'5' {
                continue;
            }
            name
        };
        if !valid_relative(rel) {
            return Err(malformed);
        }
        let mode = octal(&h[100..108]).ok_or(malformed)? as u32 & 0o777;
        total = total.saturating_add(size as u64);
        if total > max_total {
            return Err(InvalidArchive("archive exceeds read cap"));
        }
        if kind == b'1' {
            // Hard link (cargo's target/ has them): a copy of an earlier file.
            let link = field(157..257);
            let link = link.trim_start_matches("./").trim_start_matches('/');
            let target = match link.split_once('/') {
                Some((first, rest)) if container_root && name.starts_with(&format!("{first}/")) => {
                    rest
                }
                _ => link,
            };
            let data = entries
                .iter()
                .find_map(|e| match e {
                    TreeEntry::File { path, data, .. } if path == target => Some(data.clone()),
                    _ => None,
                })
                .ok_or(malformed)?;
            total = total.saturating_add(data.len() as u64);
            if total > max_total {
                return Err(InvalidArchive("archive exceeds read cap"));
            }
            entries.push(TreeEntry::File {
                path: rel.into(),
                mode,
                data,
            });
            continue;
        }
        entries.push(match kind {
            b'0' | 0 => TreeEntry::File {
                path: rel.into(),
                mode,
                data: data.to_vec(),
            },
            b'5' => TreeEntry::Dir {
                path: rel.into(),
                mode,
            },
            b'2' => TreeEntry::Symlink {
                path: rel.into(),
                target: field(157..257),
            },
            _ => return Err(malformed),
        });
    }
    if !container_root && !complete {
        return Err(malformed);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn tree_archive_round_trips_long_names_and_refuses_traversal() {
        let long = format!("{}/file.txt", "d".repeat(120));
        let entries = vec![
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
                path: long.clone(),
                mode: 0o755,
                data: vec![7; 700],
            },
            TreeEntry::Symlink {
                path: "link".into(),
                target: "src/lib.rs".into(),
            },
        ];
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
        push_member(&mut evil, "work/../../etc/passwd", 0o644, 0, b'0', "");
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
        push_member(&mut a, "/", 0o755, 0, b'5', "");
        push_member(&mut a, "/f", 0o644, 3, b'0', "");
        let start = a.len();
        a.extend_from_slice(b"abc");
        a.resize(start + padded(3), 0);
        push_member(&mut a, "/g", 0o644, 0, b'1', "/f");
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
        push_member(&mut bad, "/g", 0o644, 0, b'1', "/missing");
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
