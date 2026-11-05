//! One durable JSON record per hibernation operation, bound to the digest of
//! the payload that created it. Repeats with the same digest recover the same
//! transition; a different payload under the same record is refused.
use anyhow::{Context, Result, bail};
use aq_lifecycle_protocol::Digest;
use serde::{Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

pub trait Bound {
    fn bound_digest(&self) -> &Digest;
}

pub struct Journal {
    path: PathBuf,
}
impl Journal {
    pub fn new(dir: &Path, name: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("journal dir {}", dir.display()))?;
        Ok(Self {
            path: dir.join(format!("{name}.json")),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("hibernation journal record")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read hibernation journal"),
        }
    }
    pub fn store<T: Serialize>(&self, value: &T) -> Result<()> {
        crate::guardian::durable_replace(&self.path, &serde_json::to_vec_pretty(value)?)
    }
    /// Load the record bound to `digest`, or create it with `init` and journal
    /// it before any effect. A record bound to another digest is a conflict.
    pub fn open_bound<T: Serialize + DeserializeOwned + Bound>(
        &self,
        digest: &Digest,
        init: impl FnOnce() -> Result<T>,
    ) -> Result<(T, bool)> {
        if let Some(existing) = self.load::<T>()? {
            if existing.bound_digest() != digest {
                bail!("hibernation journal is bound to a different payload digest");
            }
            return Ok((existing, false));
        }
        let value = init()?;
        self.store(&value)?;
        Ok((value, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Rec {
        digest: Digest,
        n: u32,
    }
    impl Bound for Rec {
        fn bound_digest(&self) -> &Digest {
            &self.digest
        }
    }
    #[test]
    fn journal_recovers_same_digest_and_refuses_other() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path(), "op").unwrap();
        let a = Digest::of(b"a");
        let (first, created) = j
            .open_bound(&a, || {
                Ok(Rec {
                    digest: a.clone(),
                    n: 1,
                })
            })
            .unwrap();
        assert!(created);
        let (again, created) = j
            .open_bound(&a, || {
                Ok(Rec {
                    digest: a.clone(),
                    n: 2,
                })
            })
            .unwrap();
        assert!(!created);
        assert_eq!(first, again);
        let b = Digest::of(b"b");
        assert!(
            j.open_bound(&b, || Ok(Rec {
                digest: b.clone(),
                n: 3
            }))
            .is_err()
        );
    }
}
