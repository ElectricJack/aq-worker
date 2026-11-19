//! The harness the node driver runs in a docker-local sandbox, and the secrets of a
//! Codex ChatGPT login that must never leave it.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Claude is the default and is never written to the driver configuration, so a
/// Claude configuration keeps its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverHarness {
    #[default]
    Claude,
    Codex,
}
impl DriverHarness {
    pub fn is_claude(&self) -> bool {
        *self == Self::Claude
    }
    /// Where the in-sandbox runner writes the harness's JSONL output.
    pub fn stream_path(&self) -> &'static str {
        match self {
            Self::Claude => "/aq/out/claude.jsonl",
            Self::Codex => "/aq/out/codex.jsonl",
        }
    }
    /// The transcript file beside the driver configuration.
    pub fn transcript_file(&self) -> &'static str {
        match self {
            Self::Claude => "claude.jsonl",
            Self::Codex => "codex.jsonl",
        }
    }
    /// The harness's name in the conclusion line (`Codex exit 0`).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }
}

/// The id, access and refresh tokens of a ChatGPT login document (`auth.json`),
/// each refused wherever it would leave the sandbox. The caller validates the
/// document's shape first.
pub fn codex_login_tokens(auth: &[u8]) -> Result<Vec<Zeroizing<Vec<u8>>>> {
    let parsed: serde_json::Value = serde_json::from_slice(auth).context("Codex login JSON")?;
    let tokens = ["id_token", "access_token", "refresh_token"].map(|name| {
        parsed["tokens"][name]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.as_bytes().to_vec()))
            .context("Codex login token absent")
    });
    tokens.into_iter().collect()
}
