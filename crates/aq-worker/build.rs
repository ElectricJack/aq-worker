//! Stamps the worker with the source it was built from, so the server and the
//! owner can tell two builds of one version apart: `AQ_WORKER_VERSION` is the
//! package version, `+`, and a 12-character git hash (`-dirty` when the tree
//! had uncommitted changes). `AQ_SOURCE_REVISION` overrides the hash, as for
//! aq-server. Without either, the revision is `dev`.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=AQ_SOURCE_REVISION");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    let stamped = std::env::var("AQ_SOURCE_REVISION")
        .ok()
        .filter(|r| !r.trim().is_empty())
        .map(|r| r.trim().chars().take(40).collect::<String>());
    let revision = stamped
        .or_else(|| {
            let hash = git(&["rev-parse", "--short=12", "HEAD"]).filter(|h| !h.is_empty())?;
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            Some(format!("{hash}{}", if dirty { "-dirty" } else { "" }))
        })
        .unwrap_or_else(|| "dev".into());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rustc-env=AQ_WORKER_VERSION={version}+{revision}");
}
