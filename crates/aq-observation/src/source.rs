//! Exact host-admitted file handle. No native-home scanning, cwd fallback or
//! workspace-selected paths. All I/O and anchor checks are bounded.
use crate::{
    framing::MAX_READ_BYTES,
    lifecycle::{Digest, Text, Validate},
};
use anyhow::{Context as _, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}
impl FileIdentity {
    fn of(m: &std::fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub end: u64,
    pub bytes: u8,
    pub digest: Digest,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub offset: u64,
    pub anchor: Option<Anchor>,
}
impl Cursor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (self.offset == 0) == self.anchor.is_none(),
            "cursor anchor presence"
        );
        if let Some(anchor) = &self.anchor {
            ensure!(
                anchor.end == self.offset
                    && anchor.bytes > 0
                    && anchor.bytes <= 64
                    && u64::from(anchor.bytes) <= self.offset,
                "cursor anchor interval"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourcePermit {
    pub source_id: Uuid,
    pub root: PathBuf,
    pub member: String,
    pub native_session_key: Text,
    pub expected: FileIdentity,
}
pub struct SourceFile {
    permit: SourcePermit,
    directory: File,
    file: File,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SourceChange {
    Missing,
    Rotated,
    Truncated,
    AnchorChanged,
}
pub enum ReadOutcome {
    Ready(ReadChunk),
    Changed(SourceChange),
}
pub struct ReadChunk {
    pub start: u64,
    pub bytes: Vec<u8>,
    prefix: Vec<u8>,
}
impl ReadChunk {
    pub fn cursor_after(&self, consumed: usize) -> Result<Cursor> {
        ensure!(
            consumed > 0 && consumed <= self.bytes.len(),
            "consumed source range"
        );
        let mut tail = self.prefix.clone();
        tail.extend_from_slice(&self.bytes[..consumed]);
        let bytes = tail.len().min(64);
        let end = self
            .start
            .checked_add(consumed as u64)
            .context("source offset overflow")?;
        Ok(Cursor {
            offset: end,
            anchor: Some(Anchor {
                end,
                bytes: bytes as u8,
                digest: Digest::of(&tail[tail.len() - bytes..]),
            }),
        })
    }
}
fn member(value: &str) -> Result<std::ffi::CString> {
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && !matches!(value, "." | "..")
            && !value.contains('/')
            && !value.as_bytes().contains(&0),
        "exact source basename required"
    );
    Ok(std::ffi::CString::new(value)?)
}
fn check_file(file: &File) -> Result<std::fs::Metadata> {
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o022 == 0
            && m.nlink() == 1,
        "source file type/ownership refused"
    );
    Ok(m)
}
fn open_member(directory: &File, name: &str) -> Result<File> {
    let name = member(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    check_file(&file)?;
    Ok(file)
}
fn read_exact_at(file: &File, start: u64, length: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    let mut read = 0;
    while read < length {
        let n = file.read_at(
            &mut bytes[read..],
            start
                .checked_add(read as u64)
                .context("source range overflow")?,
        )?;
        ensure!(n > 0, "source truncated during bounded read");
        read += n;
    }
    Ok(bytes)
}
impl SourceFile {
    pub fn open(permit: SourcePermit) -> Result<Self> {
        ensure!(
            !permit.source_id.is_nil() && permit.expected.inode > 0,
            "source permit identity"
        );
        permit.native_session_key.validate()?;
        member(&permit.member)?;
        ensure!(
            permit.root.is_absolute() && std::fs::canonicalize(&permit.root)? == permit.root,
            "source root must be canonical without symlink components"
        );
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&permit.root)?;
        let m = directory.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
            "source root ownership refused"
        );
        let file = open_member(&directory, &permit.member)?;
        ensure!(
            FileIdentity::of(&file.metadata()?) == permit.expected,
            "source identity changed before open"
        );
        Ok(Self {
            permit,
            directory,
            file,
        })
    }
    /// Capture metadata only for the exact host-selected member. This helper does
    /// not discover a native key or authorize collection; the caller supplies both.
    pub fn inspect(
        source_id: Uuid,
        root: &Path,
        name: &str,
        native_session_key: Text,
    ) -> Result<SourcePermit> {
        member(name)?;
        let m = std::fs::symlink_metadata(root.join(name))?;
        ensure!(!m.file_type().is_symlink(), "source symlink refused");
        Ok(SourcePermit {
            source_id,
            root: root.into(),
            member: name.into(),
            native_session_key,
            expected: FileIdentity::of(&m),
        })
    }
    pub fn permit(&self) -> &SourcePermit {
        &self.permit
    }
    pub fn read(&self, cursor: &Cursor, limit: usize) -> Result<ReadOutcome> {
        cursor.validate()?;
        ensure!((1..=MAX_READ_BYTES).contains(&limit), "source read budget");
        let current = match open_member(&self.directory, &self.permit.member) {
            Ok(f) => f,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(ReadOutcome::Changed(SourceChange::Missing));
            }
            Err(e) => return Err(e),
        };
        let m = current.metadata()?;
        if FileIdentity::of(&m) != self.permit.expected {
            return Ok(ReadOutcome::Changed(SourceChange::Rotated));
        }
        let m = check_file(&self.file)?;
        if m.len() < cursor.offset {
            return Ok(ReadOutcome::Changed(SourceChange::Truncated));
        }
        let prefix_length = cursor.offset.min(64) as usize;
        let prefix = read_exact_at(
            &self.file,
            cursor.offset - prefix_length as u64,
            prefix_length,
        )?;
        if let Some(anchor) = &cursor.anchor
            && (anchor.bytes as usize != prefix.len() || Digest::of(&prefix) != anchor.digest)
        {
            return Ok(ReadOutcome::Changed(SourceChange::AnchorChanged));
        }
        let length = (m.len() - cursor.offset).min(limit as u64) as usize;
        let bytes = read_exact_at(&self.file, cursor.offset, length)?;
        Ok(ReadOutcome::Ready(ReadChunk {
            start: cursor.offset,
            bytes,
            prefix,
        }))
    }
}
