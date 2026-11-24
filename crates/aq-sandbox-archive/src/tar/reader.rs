//! Bounded reading of regular files and Docker directory archives.
use super::{BLOCK, InvalidArchive, TreeEntry, padded, valid_relative};
const MALFORMED: InvalidArchive = InvalidArchive("tree archive malformed or unsafe");
const READ_CAP: InvalidArchive = InvalidArchive("archive exceeds read cap");

pub(super) fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

struct ReadHeader<'a>(&'a [u8]);

impl ReadHeader<'_> {
    fn size(&self) -> Option<u64> {
        let header = self.0;
        let stored = octal(&header[148..156])?;
        let sum: u64 = header[..148]
            .iter()
            .chain(&header[156..])
            .map(|byte| u64::from(*byte))
            .sum();
        (stored == sum + 8 * 32)
            .then(|| octal(&header[124..136]))
            .flatten()
    }

    fn name(&self) -> String {
        let header = self.0;
        let prefix = field(&header[345..500]);
        let name = field(&header[..100]);
        if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        }
    }

    fn mode(&self) -> Result<u32, InvalidArchive> {
        Ok(octal(&self.0[100..108]).ok_or(MALFORMED)? as u32 & 0o777)
    }

    fn link(&self) -> String {
        field(&self.0[157..257])
    }

    fn kind(&self) -> u8 {
        self.0[156]
    }
}

/// Returns the content of the archive's single regular file, skipping PAX and
/// GNU long-name records. Fails closed above `max` bytes or on a bad header.
pub fn single_file(archive: &[u8], max: u64) -> Result<Vec<u8>, InvalidArchive> {
    let malformed = InvalidArchive("archive is not a single regular file");
    let mut offset = 0;
    while let Some(header) = archive.get(offset..offset + BLOCK) {
        let header = ReadHeader(header);
        let size = header.size().ok_or(malformed)?;
        match header.kind() {
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

fn field(header: &[u8]) -> String {
    let end = header
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(header.len());
    String::from_utf8_lossy(&header[..end]).into_owned()
}

struct TreeReader {
    entries: Vec<TreeEntry>,
    long_name: Option<String>,
    remaining: u64,
    /// Container archives carry a directory prefix; volume exports are root-relative.
    container_root: bool,
}

impl TreeReader {
    fn charge(&mut self, size: usize) -> Result<(), InvalidArchive> {
        self.remaining = self.remaining.checked_sub(size as u64).ok_or(READ_CAP)?;
        Ok(())
    }

    fn read_entry(&mut self, header: &ReadHeader<'_>, data: &[u8]) -> Result<(), InvalidArchive> {
        match header.kind() {
            b'L' | b'x' => return self.read_long_name(header.kind(), data),
            b'g' => return Ok(()),
            _ => {}
        }
        let name = self.long_name.take().unwrap_or_else(|| header.name());
        let name = name.trim_start_matches("./").trim_end_matches('/');
        let rel = if self.container_root {
            let Some((_, rel)) = name.split_once('/') else {
                return Ok(()); // the archived directory itself
            };
            rel
        } else {
            if (name == "." || name.is_empty()) && header.kind() == b'5' {
                return Ok(());
            }
            name
        };
        if !valid_relative(rel) {
            return Err(MALFORMED);
        }
        let mode = header.mode()?;
        self.charge(data.len())?;
        let path = rel.to_owned();
        let entry = match header.kind() {
            b'0' | 0 => TreeEntry::File {
                path,
                mode,
                data: data.to_vec(),
            },
            b'5' => TreeEntry::Dir { path, mode },
            b'2' => TreeEntry::Symlink {
                path,
                target: header.link(),
            },
            b'1' => {
                let file = self.hard_link(name, &header.link())?;
                TreeEntry::File {
                    path,
                    mode,
                    data: file,
                }
            }
            _ => return Err(MALFORMED),
        };
        self.entries.push(entry);
        Ok(())
    }

    fn read_long_name(&mut self, kind: u8, data: &[u8]) -> Result<(), InvalidArchive> {
        if data.len() > 64 * 1024 {
            return Err(MALFORMED);
        }
        if kind == b'L' {
            self.long_name = Some(
                String::from_utf8_lossy(data)
                    .trim_end_matches('\0')
                    .to_owned(),
            );
        } else if let Some(path) = pax_path(data) {
            self.long_name = Some(path);
        }
        Ok(())
    }

    fn hard_link(&mut self, name: &str, link: &str) -> Result<Vec<u8>, InvalidArchive> {
        let link = link.trim_start_matches("./").trim_start_matches('/');
        let target = match link.split_once('/') {
            Some((first, rest))
                if self.container_root && name.starts_with(&format!("{first}/")) =>
            {
                rest
            }
            _ => link,
        };
        let data = self
            .entries
            .iter()
            .find_map(|entry| match entry {
                TreeEntry::File { path, data, .. } if path == target => Some(data),
                _ => None,
            })
            .ok_or(MALFORMED)?;
        self.remaining = self
            .remaining
            .checked_sub(data.len() as u64)
            .ok_or(READ_CAP)?;
        Ok(data.clone())
    }
}

/// Reads a Docker `GET archive` of a directory. The leading component (the
/// directory's own name) is stripped. Unsafe or unsupported members are refused,
/// never skipped silently; hard links, devices and FIFOs are refused.
pub fn read_tree(archive: &[u8], max_total: u64) -> Result<Vec<TreeEntry>, InvalidArchive> {
    read_workspace_tree(archive, max_total, true)
}

/// Container archives carry a directory prefix. Volume exports have root-relative
/// paths and must end with the two zero blocks, so a partial export is refused.
pub fn read_workspace_tree(
    archive: &[u8],
    max_total: u64,
    container_root: bool,
) -> Result<Vec<TreeEntry>, InvalidArchive> {
    let mut reader = TreeReader {
        entries: Vec::new(),
        long_name: None,
        remaining: max_total,
        container_root,
    };
    let mut offset = 0;
    let mut complete = false;
    while let Some(header) = archive.get(offset..offset + BLOCK) {
        if header.iter().all(|b| *b == 0) {
            complete = archive
                .get(offset + BLOCK..offset + 2 * BLOCK)
                .is_some_and(|next| next.iter().all(|b| *b == 0));
            break;
        }
        let header = ReadHeader(header);
        let size = header.size().ok_or(MALFORMED)? as usize;
        let start = offset + BLOCK;
        let data = archive.get(start..start + size).ok_or(MALFORMED)?;
        reader.read_entry(&header, data)?;
        offset = start + padded(size);
    }
    if !container_root && !complete {
        return Err(MALFORMED);
    }
    Ok(reader.entries)
}
