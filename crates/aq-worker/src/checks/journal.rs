//! Durable per-operation check records. A record exists before any sandbox is
//! created, and its stop is persisted before it is sent and never replaced, so
//! a restart can neither rerun a check nor report a second, different stop.
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::StopReport;
use crate::backend::SandboxIdentity;

const MAX_RECORD_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Not yet acknowledged; resend the identical stop.
    Pending,
    Recorded,
    /// The server refused it (409/404/400). Never resent automatically.
    Refused {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub operation_id: Uuid,
    pub inputs_digest: String,
    /// Recorded before create, so a crash can always find the sandbox.
    pub sandbox: Option<SandboxIdentity>,
    pub stop: Option<StopReport>,
    pub delivery: Delivery,
    /// The sandbox was destroyed after the stop was recorded.
    pub released: bool,
}
impl Record {
    pub fn new(operation_id: Uuid, inputs_digest: String) -> Self {
        Self {
            operation_id,
            inputs_digest,
            sandbox: None,
            stop: None,
            delivery: Delivery::Pending,
            released: false,
        }
    }
    /// Sets the one stop. An existing stop always wins.
    pub fn decide(&mut self, stop: StopReport) -> &StopReport {
        self.stop.get_or_insert(stop)
    }
}

pub struct Journal {
    dir: PathBuf,
    _lock: File,
}
impl Journal {
    /// Opens (creating 0700) the directory and holds its exclusive lock for
    /// the life of the value; a second runner on the same journal is refused.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).context("create check journal")?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(".lock"))
            .context("open check journal lock")?;
        lock.try_lock_exclusive()
            .context("check journal is held by another runner")?;
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    fn path(&self, operation: Uuid) -> PathBuf {
        self.dir.join(format!("{}.json", operation.as_hyphenated()))
    }

    pub fn load(&self, operation: Uuid) -> Result<Option<Record>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(operation))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("open check record"),
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            "check record too large"
        );
        let record: Record = serde_json::from_slice(&bytes).context("corrupt check record")?;
        ensure!(
            record.operation_id == operation,
            "check record identity mismatch"
        );
        Ok(Some(record))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        if let Some(stop) = &record.stop
            && stop.operation_id != record.operation_id
        {
            bail!("check stop names another operation");
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(&serde_json::to_vec(record)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.path(record.operation_id))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// Every record not yet fully settled (unsent stop or unreleased sandbox).
    pub fn unsettled(&self) -> Result<Vec<Record>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let Some(op) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| Uuid::parse_str(n).ok())
            else {
                continue;
            };
            if let Some(record) = self.load(op)?
                && (record.delivery == Delivery::Pending
                    || (record.delivery == Delivery::Recorded
                        && !record.released
                        && record.sandbox.is_some()))
            {
                out.push(record);
            }
        }
        out.sort_by_key(|r| r.operation_id);
        Ok(out)
    }
}
