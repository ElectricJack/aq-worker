//! The Codex helper programs a sandbox session needs beside the Codex executable
//! (its shell tool does not start without them). One pinned source for every
//! Codex launch, hand-started or queued: `packaging/native-runtime/pins.json`.
use crate::tar::TreeEntry;
use anyhow::{Context, Result, ensure};
use sha2::Digest as _;
use std::{io::Read, path::Path};

/// The files under `/aq/in` of a session: `codex-digest` (the pinned Codex
/// executable these helpers belong to) and `codex-runtime/`. Only these public,
/// exact-version artifacts may enter a session; other bytes are refused.
pub fn bundle(path: &Path) -> Result<Vec<TreeEntry>> {
    let file = |path: &str, data: Vec<u8>| TreeEntry::File {
        path: path.into(),
        mode: 0o600,
        data,
    };
    let pins: serde_json::Value =
        serde_json::from_str(include_str!("../../../packaging/native-runtime/pins.json"))?;
    let digest = pins["inputs"]["codex"]["sha256"].as_str();
    let mut entries = vec![file(
        "codex-digest",
        digest.context("Codex executable pin")?.into(),
    )];
    for name in [
        "codex-runtime",
        "codex-runtime/codex-path",
        "codex-runtime/codex-resources",
    ] {
        entries.push(TreeEntry::Dir {
            path: name.into(),
            mode: 0o700,
        });
    }
    for name in [
        "codex-code-mode-host",
        "codex-path/rg",
        "codex-resources/bwrap",
    ] {
        let pin = &pins["inputs"][name];
        let expected = pin["bytes"].as_u64().context("Codex helper size")?;
        let helper = std::fs::File::open(path.join(name)).context("Codex helper unavailable")?;
        ensure!(
            helper.metadata()?.is_file() && helper.metadata()?.len() == expected,
            "Codex helper size mismatch"
        );
        let mut bytes = Vec::new();
        helper.take(expected + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == expected
                && Some(format!("{:x}", sha2::Sha256::digest(&bytes)).as_str())
                    == pin["sha256"].as_str(),
            "Codex helper digest mismatch"
        );
        entries.push(file(&format!("codex-runtime/{name}"), bytes));
    }
    Ok(entries)
}
