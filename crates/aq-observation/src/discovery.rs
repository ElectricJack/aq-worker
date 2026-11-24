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
impl Resolution {
    fn from_identity(permit: SourcePermit, matches: bool) -> Self {
        if matches {
            Self::Found(permit)
        } else {
            Self::IdentityMismatch
        }
    }
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
        self.validate()?;
        let key = self.native_session_key.as_str();
        if self.format == Format::OpenCodeSessionDto6c24ec9 {
            return Ok(Resolution::Unsupported);
        }
        if let Some(member) = self.direct_member(key) {
            return self.probe(&member, owner_uid);
        }
        let Some(candidates) = self.candidates(key)? else {
            return Ok(Resolution::BudgetExceeded);
        };
        self.probe_candidates(candidates, owner_uid)
    }

    fn direct_member(&self, key: &str) -> Option<String> {
        if let Some(member) = &self.exact_member {
            return Some(member.clone());
        }
        let direct = format!("{key}.jsonl");
        match self.format {
            Format::ClaudeV1Jsonl9a9b8e8 => Some(direct),
            Format::GeminiRecording6814e00 if self.root.join(&direct).exists() => Some(direct),
            _ => None,
        }
    }

    fn validate(&self) -> Result<()> {
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
        Ok(())
    }

    fn candidates(&self, key: &str) -> Result<Option<Vec<String>>> {
        let mut candidates = Vec::new();
        for (index, entry) in std::fs::read_dir(&self.root)?.enumerate() {
            if index >= self.maximum_entries {
                return Ok(None);
            }
            let name = entry?.file_name();
            if let Some(name) = name
                .to_str()
                .filter(|name| self.matches_candidate(name, key))
            {
                candidates.push(name.to_owned());
            }
            if candidates.len() > self.maximum_candidates {
                return Ok(None);
            }
        }
        Ok(Some(candidates))
    }

    fn matches_candidate(&self, name: &str, key: &str) -> bool {
        match self.format {
            Format::CodexRollout67f0994 => {
                name.starts_with("rollout-") && name.ends_with(&format!("-{key}.jsonl"))
            }
            Format::GeminiRecording6814e00 => {
                name.starts_with("session-")
                    && name.ends_with(&format!("-{}.jsonl", &key[..key.len().min(8)]))
            }
            _ => false,
        }
    }

    fn probe_candidates(
        &self,
        candidates: Vec<String>,
        owner_uid: Option<u32>,
    ) -> Result<Resolution> {
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
        self.verify_record_identity(permit, &read.bytes)
    }

    fn verify_record_identity(&self, permit: SourcePermit, bytes: &[u8]) -> Result<Resolution> {
        let framed = frame(bytes, 0, false)?;
        for row in framed.records {
            if row.oversized {
                return Ok(Resolution::BudgetExceeded);
            }
            if let Some(is_match) = self.session_matches(&row.bytes) {
                return Ok(Resolution::from_identity(permit, is_match));
            }
        }
        Ok(if framed.partial || bytes.is_empty() {
            Resolution::NotReady
        } else {
            Resolution::BudgetExceeded
        })
    }

    fn session_matches(&self, bytes: &[u8]) -> Option<bool> {
        let value = crate::json::parse(bytes).ok()?;
        self.record_key(&value)
            .map(|key| key == self.native_session_key.as_str())
    }

    fn record_key<'a>(&self, value: &'a serde_json::Value) -> Option<&'a str> {
        match self.format {
            Format::CodexRollout67f0994 if value["type"] == "session_meta" => {
                value["payload"]["id"].as_str()
            }
            Format::ClaudeV1Jsonl9a9b8e8 => value["sessionId"].as_str(),
            Format::GeminiRecording6814e00 => value["sessionId"]
                .as_str()
                .or_else(|| value["$set"]["sessionId"].as_str()),
            _ => None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
