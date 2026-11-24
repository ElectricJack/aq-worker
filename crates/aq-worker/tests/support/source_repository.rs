use super::*;
use std::process::Command;
pub(super) struct Fixture {
    pub(super) exe: std::path::PathBuf,
    pub(super) pin: String,
    pub(super) central: std::path::PathBuf,
    pub(super) rid: Uuid,
    pub(super) binding: SourceBinding,
    pub(super) exported: Value,
    pub(super) checkpoint: Value,
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

fn export_base(exe: &Path, central: &Path, rid: Uuid, repo: &Path) -> (Value, Value, Value) {
    let git = GitRepository {
        exe,
        root: central,
        repo: rid,
    };
    let oid = json!({"algorithm":"sha1","hex":initialize_repo(repo)});
    git.request("repo.register",
        json!({"binding":{"identity":"fixture","mode":"central","binding_generation":1,"remote_url":repo,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}}),
        0,
    );
    git.request("repo.sync", json!({}), 1);
    let w = git.request("workspace.create", json!({"base":oid}), 1);
    let c = git.request("workspace.checkpoint",
        json!({"workspace_id":w["workspace_id"],"expected_generation":w["generation"],"expected_head":oid,"quiescent":true}),
        1,
    );
    let exported = git.request(
        "artifact.export",
        json!({"base":oid,"source":c["source"],"retention_handle":c["retention_handle"]}),
        1,
    );
    (oid, c, exported)
}

pub(super) fn register_worker(exe: &Path, worker: &Path, rid: Uuid) {
    GitRepository {
        exe,
        root: worker,
        repo: rid,
    }
    .request(
        "repo.register",
        json!({"binding":{"identity":"fixture","mode":"worker","binding_generation":1}}),
        0,
    );
}

pub(super) fn create_fixture(exe: &Path, root: &Path) -> Arc<Fixture> {
    let central = root.join("central");
    let repo = root.join("repository");
    let rid = Uuid::new_v4();
    let (oid, checkpoint, exported) = export_base(exe, &central, rid, &repo);
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
        checkpoint,
    })
}
pub(super) fn copy_artifact(from: &Path, to: &Path, handle: &str) {
    let source = from.join("artifacts").join(handle);
    let destination = to.join("artifacts/incoming").join(handle);
    std::fs::create_dir_all(&destination).unwrap();
    for name in ["manifest.json", "objects.bundle"] {
        std::fs::copy(source.join(name), destination.join(name)).unwrap();
    }
}
#[path = "source_repository/prepared.rs"]
mod prepared;
pub(super) use prepared::PreparedSource;
struct GitRepository<'a> {
    exe: &'a Path,
    root: &'a Path,
    repo: Uuid,
}
impl GitRepository<'_> {
    fn request(&self, operation: &str, params: Value, generation: u64) -> Value {
        use std::process::{Command, Stdio};
        let mut child = Command::new(self.exe)
            .arg("--state-dir")
            .arg(self.root)
            .args(["request", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let id = Uuid::new_v4();
        let request = json!({"protocol_version":1,"request_id":id.to_string(),"operation":operation,"operation_id":id,"repository_id":self.repo,"expected_binding_generation":generation,"params":params});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "{}", response);
        response["result"].clone()
    }
}
impl Fixture {
    pub(super) fn central_request(&self, operation: &str, params: Value, generation: u64) -> Value {
        GitRepository {
            exe: &self.exe,
            root: &self.central,
            repo: self.rid,
        }
        .request(operation, params, generation)
    }
}
