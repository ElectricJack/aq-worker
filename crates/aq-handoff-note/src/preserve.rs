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
        "{PRESERVED_PREFIX}{sandbox} preserved; Changes: unknown; Runner failure: {error:#}"
    ))
}
/// How a preserved claim's conclusion starts (before any folding can cut it).
const PRESERVED_PREFIX: &str = "Sandbox ";
/// [`preserved_conclusion`] of a recorded claim's bytes (a result submission). Bytes that do
/// not decode count as preserved: the sandbox is kept.
pub fn preserved_claim_bytes(submission: &[u8]) -> bool {
    let value: Option<serde_json::Value> = serde_json::from_slice(submission).ok();
    value
        .as_ref()
        .and_then(|v| v["conclusion"].as_str())
        .is_none_or(preserved_conclusion)
}
/// Whether a recorded claim's conclusion is the driver's claim for a preserved sandbox. The
/// journal keeps the claim durably, so this survives the loss of the driver dir's marker (the
/// runtime dir is gone after a reboot). An agent's conclusion that reads the same way keeps a
/// sandbox (safe); it can never make a preserved one destroyable.
pub fn preserved_conclusion(conclusion: &str) -> bool {
    let current = conclusion
        .strip_prefix(PRESERVED_PREFIX)
        .is_some_and(|rest| {
            rest.split_once(' ')
                .is_some_and(|(_, tail)| tail.starts_with("preserved;"))
        });
    // Earlier drivers put the statement last.
    let earlier = conclusion.starts_with("Changes: unknown; Runner failure: ")
        && (conclusion.ends_with(" preserved") || conclusion.ends_with("..."));
    current || earlier
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
        let claim = preserved_claim(&refused, "sb", &config).unwrap();
        assert!(claim.starts_with("Sandbox sb preserved; "), "{claim}");
        assert!(preserved_conclusion(&claim));
        let long = anyhow::anyhow!("{}", "x".repeat(900));
        let folded = crate::fold_claim(preserved_claim(&long, "sb", &config).unwrap());
        assert!(preserved_conclusion(&folded), "the prefix survives folding");
        assert!(preserved(host.path(), "a-1") && !preserved(host.path(), "a-2"));
        assert!(!preserved(&host.path().join("absent"), "a-1"));
    }
    /// The preserved rule from durable state: after the runtime dir (and every driver
    /// marker) is wiped, a preserved claim still keeps its sandbox and an ordinary claim
    /// still releases its sandbox.
    #[test]
    fn preserved_claims_are_known_without_the_runtime_dir() {
        let host = tempfile::tempdir().unwrap();
        let dir = host.path().join("driver-1");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("driver.json");
        std::fs::write(&config, r#"{"owner":{"attempt_id":"a-1"}}"#).unwrap();
        let kept = preserved_claim(&anyhow::anyhow!("copy-back refused"), "sb", &config).unwrap();
        let done = crate::handoff_note(aq_protocol::HandoffReason::Restart, &[], &[], &[]);
        std::fs::remove_dir_all(host.path()).unwrap();
        assert!(
            !preserved(host.path(), "a-1"),
            "the marker went with the runtime dir"
        );
        assert!(preserved_conclusion(&crate::fold_claim(kept)));
        assert!(!preserved_conclusion(&done));
        assert!(!preserved_conclusion(
            "All steps written; sandbox sb preserved for review? no."
        ));
        let earlier = "Changes: unknown; Runner failure: copy-back refused; sandbox sb preserved";
        assert!(preserved_conclusion(earlier));
        let claim = |text: &str| serde_json::json!({"schema_version": 1, "conclusion": text});
        let bytes = |text: &str| serde_json::to_vec(&claim(text)).unwrap();
        assert!(preserved_claim_bytes(&bytes(
            "Sandbox sb preserved; Changes: unknown"
        )));
        assert!(!preserved_claim_bytes(&bytes(&done)));
        assert!(
            preserved_claim_bytes(b"not json"),
            "unreadable keeps the sandbox"
        );
    }
}
