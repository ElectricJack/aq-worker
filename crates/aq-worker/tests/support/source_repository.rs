use super::*;
use std::process::Command;
pub(super) struct Fixture {
    pub(super) exe: std::path::PathBuf,
    pub(super) pin: String,
    pub(super) central: std::path::PathBuf,
    pub(super) rid: Uuid,
    pub(super) binding: SourceBinding,
    pub(super) exported: Value,
}

fn initialize_repo(repo: &Path) -> String {
    std::fs::create_dir(repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .arg(repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base"
            ])
            .status()
            .unwrap()
            .success()
    );
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned()
}

fn export_base(exe: &Path, central: &Path, rid: Uuid, repo: &Path) -> (Value, Value) {
    let oid = json!({"algorithm":"sha1","hex":initialize_repo(repo)});
    direct(
        exe,
        central,
        rid,
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"central","binding_generation":1,"remote_url":repo,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}}),
        0,
    );
    direct(exe, central, rid, "repo.sync", json!({}), 1);
    let w = direct(
        exe,
        central,
        rid,
        "workspace.create",
        json!({"base":oid}),
        1,
    );
    let c = direct(
        exe,
        central,
        rid,
        "workspace.checkpoint",
        json!({"workspace_id":w["workspace_id"],"expected_generation":w["generation"],"expected_head":oid,"quiescent":true}),
        1,
    );
    let exported = direct(
        exe,
        central,
        rid,
        "artifact.export",
        json!({"base":oid,"source":c["source"],"retention_handle":c["retention_handle"]}),
        1,
    );
    (oid, exported)
}

pub(super) fn register_worker(exe: &Path, worker: &Path, rid: Uuid) {
    direct(
        exe,
        worker,
        rid,
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"worker","binding_generation":1}}),
        0,
    );
}

pub(super) fn create_fixture(exe: &Path, root: &Path) -> Arc<Fixture> {
    let central = root.join("central");
    let repo = root.join("repository");
    let rid = Uuid::new_v4();
    let (oid, exported) = export_base(exe, &central, rid, &repo);
    use sha2::{Digest, Sha256};
    let pin = format!("{:x}", Sha256::digest(std::fs::read(exe).unwrap()));
    Arc::new(Fixture {
        exe: exe.to_owned(),
        pin,
        central,
        rid,
        binding: SourceBinding {
            repository_id: rid,
            binding_generation: 1,
            logical_identity: "fixture".into(),
            base: serde_json::from_value(oid).unwrap(),
        },
        exported,
    })
}
