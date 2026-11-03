//! Minimal ustar writer/reader for the Docker archive API. Material archives
//! are built in one pre-sized zeroizing buffer, so no reallocation leaves a copy.
use crate::backend::BackendError;
use zeroize::Zeroizing;

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
) -> Result<Zeroizing<Vec<u8>>, BackendError> {
    let relative = path
        .strip_prefix('/')
        .filter(|rest| rest.len() < 100 && rest.contains('/'))
        .ok_or(BackendError::InvalidRequest(
            "material path unsupported by archive",
        ))?;
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
pub fn single_file(archive: &[u8], max: u64) -> Result<Vec<u8>, BackendError> {
    let malformed = BackendError::InvalidRequest("archive is not a single regular file");
    let mut offset = 0;
    while let Some(header) = archive.get(offset..offset + BLOCK) {
        let stored = octal(&header[148..156]).ok_or(malformed.clone())?;
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
        let size = octal(&header[124..136]).ok_or(malformed.clone())?;
        match header[156] {
            b'0' | 0 => {
                if size > max {
                    return Err(BackendError::InvalidRequest("archive exceeds read cap"));
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
            Err(BackendError::InvalidRequest("archive exceeds read cap"))
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
}
