//! Bounded discovery inside an explicitly admitted native directory. A known
//! native session key is mandatory; never choose newest/cwd-similar history.
use crate::{
    Format,
    framing::{MAX_READ_BYTES, frame},
    lifecycle::Text,
    source::{Cursor, ReadOutcome, SourceFile, SourcePermit},
};
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use uuid::Uuid;
#[derive(Clone, Debug)]
pub struct Discovery {
    pub source_id: Uuid,
    pub root: PathBuf,
    pub native_session_key: Text,
    pub format: Format,
    pub exact_member: Option<String>,
    pub maximum_entries: usize,
    pub maximum_candidates: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Found(SourcePermit),
    Missing,
    NotReady,
    Ambiguous,
    BudgetExceeded,
    IdentityMismatch,
    Unsupported,
}
impl Discovery {
    pub fn resolve(&self) -> Result<Resolution> {
        self.resolve_owned(None)
    }
    /// A trusted runner may admit its separately owned agent transcript.
    pub fn resolve_for_owner(&self, owner_uid: u32) -> Result<Resolution> {
        self.resolve_owned(Some(owner_uid))
    }
    fn resolve_owned(&self, owner_uid: Option<u32>) -> Result<Resolution> {
        ensure!(
            !self.source_id.is_nil()
                && (1..=4096).contains(&self.maximum_entries)
                && (1..=32).contains(&self.maximum_candidates),
            "discovery bound"
        );
        let key = self.native_session_key.as_str();
        ensure!(
            !key.is_empty()
                && key.len() <= 128
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "native session key is not a safe exact identifier"
        );
        ensure!(
            self.root.is_absolute() && std::fs::canonicalize(&self.root)? == self.root,
            "canonical admitted native directory required"
        );
        if self.format == Format::OpenCodeSessionDto6c24ec9 {
            return Ok(Resolution::Unsupported);
        }
        if let Some(member) = &self.exact_member {
            return self.probe(member, owner_uid);
        }
        if self.format == Format::ClaudeV1Jsonl9a9b8e8 {
            return self.probe(&format!("{key}.jsonl"), owner_uid);
        }
        if self.format == Format::GeminiRecording6814e00 {
            let direct = format!("{key}.jsonl");
            if self.root.join(&direct).exists() {
                return self.probe(&direct, owner_uid);
            }
        }
        let mut entries = 0;
        let mut candidates = vec![];
        for entry in std::fs::read_dir(&self.root)? {
            entries += 1;
            if entries > self.maximum_entries {
                return Ok(Resolution::BudgetExceeded);
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let matches = match self.format {
                Format::CodexRollout67f0994 => {
                    name.starts_with("rollout-") && name.ends_with(&format!("-{key}.jsonl"))
                }
                Format::GeminiRecording6814e00 => {
                    name.starts_with("session-")
                        && name.ends_with(&format!("-{}.jsonl", &key[..key.len().min(8)]))
                }
                _ => false,
            };
            if matches {
                candidates.push(name.to_owned());
                if candidates.len() > self.maximum_candidates {
                    return Ok(Resolution::BudgetExceeded);
                }
            }
        }
        let mut found = vec![];
        let mut not_ready = false;
        for candidate in candidates {
            match self.probe(&candidate, owner_uid)? {
                Resolution::Found(permit) => found.push(permit),
                Resolution::NotReady => not_ready = true,
                Resolution::IdentityMismatch => {}
                Resolution::Missing => {}
                other => return Ok(other),
            }
        }
        Ok(match found.len() {
            0 if not_ready => Resolution::NotReady,
            0 => Resolution::Missing,
            1 => Resolution::Found(found.remove(0)),
            _ => Resolution::Ambiguous,
        })
    }
    fn probe(&self, member: &str, owner_uid: Option<u32>) -> Result<Resolution> {
        let mut permit = match SourceFile::inspect(
            self.source_id,
            &self.root,
            member,
            self.native_session_key.clone(),
        ) {
            Ok(p) => p,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(Resolution::Missing);
            }
            Err(e) => return Err(e),
        };
        permit.owner_uid = owner_uid;
        let source = SourceFile::open(permit.clone())?;
        let ReadOutcome::Ready(read) = source.read(&Cursor::default(), MAX_READ_BYTES)? else {
            return Ok(Resolution::NotReady);
        };
        let framed = frame(&read.bytes, 0, false)?;
        for row in framed.records {
            if row.oversized {
                return Ok(Resolution::BudgetExceeded);
            }
            let value = match crate::json::parse(&row.bytes) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let key = match self.format {
                Format::CodexRollout67f0994 if value["type"] == "session_meta" => {
                    value["payload"]["id"].as_str()
                }
                Format::ClaudeV1Jsonl9a9b8e8 => value["sessionId"].as_str(),
                Format::GeminiRecording6814e00 => value["sessionId"]
                    .as_str()
                    .or_else(|| value["$set"]["sessionId"].as_str()),
                _ => None,
            };
            if let Some(key) = key {
                return Ok(if key == self.native_session_key.as_str() {
                    Resolution::Found(permit)
                } else {
                    Resolution::IdentityMismatch
                });
            }
        }
        Ok(if framed.partial || read.bytes.is_empty() {
            Resolution::NotReady
        } else {
            Resolution::BudgetExceeded
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}
