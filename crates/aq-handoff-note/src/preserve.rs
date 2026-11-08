//! What the driver leaves beside its `driver.json` for the node: the exit record, and the
//! marker of a sandbox it preserved because the work in it was never copied back.
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path};

/// Written before a preserved claim. The node never destroys such a sandbox; the operator
/// releases it (`release_preserved_sandbox`).
pub const PRESERVED_MARKER: &str = "preserved";

/// A copy-back that failed after it began writing: the worktree holds part of the sandbox's
/// tree, so no result may be claimed from it (the preserved sandbox is salvaged instead).
#[derive(Debug)]
pub struct CopyBackIncomplete;
impl std::fmt::Display for CopyBackIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("copy-back incomplete")
    }
}

fn create(path: &Path, body: &[u8]) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(body).and_then(|()| f.sync_all()))
}

/// `exit.json` beside the driver config: success means the runner exited 0
/// (backend-observed) and the workspace was copied back.
pub fn write_exit(config_path: &Path, success: bool) {
    if let Some(dir) = config_path.parent() {
        let body = serde_json::json!({ "success": success }).to_string();
        let _ = create(&dir.join("exit.json"), body.as_bytes());
    }
}

/// The failed claim for a preserved sandbox, after its `PRESERVED_MARKER` is written beside
/// `config_path`: a recorded (failed) result lets settlement send the task to rework instead
/// of a stopped attempt with no report holding it. None after an incomplete copy-back (the
/// half-synced worktree must never become a result), or when the marker cannot be written
/// (unclaimed, the sandbox is salvaged).
pub fn preserved_claim(error: &anyhow::Error, sandbox: &str, config_path: &Path) -> Option<String> {
    if error.downcast_ref::<CopyBackIncomplete>().is_some() {
        return None;
    }
    let marker = config_path.parent()?.join(PRESERVED_MARKER);
    match create(&marker, b"") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    Some(format!(
        "Changes: unknown; Runner failure: {error:#}; sandbox {sandbox} preserved"
    ))
}

/// Whether a driver of `attempt` under `host_root` (`driver-*/driver.json`) preserved its
/// sandbox. Unknown (an unreadable host root) counts as preserved.
pub fn preserved(host_root: &Path, attempt: &str) -> bool {
    let entries = match std::fs::read_dir(host_root) {
        Ok(entries) => entries,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    entries.flatten().any(|entry| {
        let dir = entry.path();
        let owner = std::fs::read(dir.join("driver.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        owner.is_some_and(|v| v["owner"]["attempt_id"] == attempt)
            && dir.join(PRESERVED_MARKER).exists()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preserved_claim_is_marked_first_and_found_by_attempt() {
        let host = tempfile::tempdir().unwrap();
        let dir = host.path().join("driver-1");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("driver.json");
        std::fs::write(&config, r#"{"owner":{"attempt_id":"a-1"}}"#).unwrap();
        assert!(!preserved(host.path(), "a-1"));
        let incomplete =
            anyhow::Error::new(std::io::Error::other("disk")).context(CopyBackIncomplete);
        assert_eq!(preserved_claim(&incomplete, "sb", &config), None);
        assert!(
            !preserved(host.path(), "a-1"),
            "an unclaimed copy-back is not preserved"
        );
        let refused = anyhow::anyhow!("copy-back symlink target must stay inside");
        assert!(
            preserved_claim(&refused, "sb", &config)
                .unwrap()
                .contains("sandbox sb preserved")
        );
        assert!(preserved(host.path(), "a-1") && !preserved(host.path(), "a-2"));
        assert!(!preserved(&host.path().join("absent"), "a-1"));
    }
}
