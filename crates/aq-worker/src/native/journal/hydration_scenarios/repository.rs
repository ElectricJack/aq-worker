fn pinned_hydration_git() -> (PathBuf, String) {
    let exe =
        PathBuf::from(std::env::var_os("AQ_GIT_TEST_BINARY").expect("explicit public Git binary"));
    let pin = std::env::var("AQ_GIT_TEST_SHA256").expect("explicit binary SHA256");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&exe).unwrap())),
        pin
    );
    assert_eq!(pin.len(), 64);
    (exe, pin)
}
fn committed_fixture(repository: &Path) -> Value {
    use std::process::Command;
    fs::create_dir(repository).unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "-q", "--initial-branch=main"])
            .arg(repository)
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        repository.join("native-input.txt"),
        b"synthetic native source bytes\n",
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(repository)
            .args(["add", "native-input.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(repository)
            .args([
                "-c",
                "user.name=SyntheticFixture",
                "-c",
                "user.email=fixture@invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "native source base"
            ])
            .status()
            .unwrap()
            .success()
    );
    let hex = String::from_utf8(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    json!({"algorithm":"sha1","hex":hex})
}
struct CentralRepository<'a> {
    exe: &'a Path,
    root: &'a Path,
    id: Uuid,
}
impl CentralRepository<'_> {
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
        let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"operation_id":id,"repository_id":self.id,"expected_binding_generation":generation,"params":params});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            output.status.success() && value["ok"] == true,
            "public Git fixture failed: {value}"
        );
        value["result"].clone()
    }
    fn export(&self, repository: &Path, oid: Value) -> Export {
        let binding = json!({
            "identity": "synthetic", "mode": "central", "binding_generation": 1,
            "remote_url": repository, "allow_file_transport": true, "allowed_refs": ["refs/heads/main"]
        });
        self.request("repo.register", json!({"binding":binding}), 0);
        self.request("repo.sync", json!({}), 1);
        let workspace = self.request("workspace.create", json!({"base":oid}), 1);
        let checkpoint = self.request("workspace.checkpoint", json!({
            "workspace_id":workspace["workspace_id"], "expected_generation":workspace["generation"],
            "expected_head":oid, "quiescent":true
        }), 1);
        let exported = self.request("artifact.export", json!({
            "base":oid, "source":checkpoint["source"], "retention_handle":checkpoint["retention_handle"]
        }), 1);
        let artifact = Uuid::parse_str(exported["artifact_handle"].as_str().unwrap()).unwrap();
        self.read_export(artifact, oid)
    }
    fn read_export(&self, artifact: Uuid, oid: Value) -> Export {
        let members = self.root.join("artifacts").join(artifact.to_string());
        let manifest_bytes = fs::read(members.join("manifest.json")).unwrap();
        let bundle = fs::read(members.join("objects.bundle")).unwrap();
        let native: GitManifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(!native.reference_only && !bundle.is_empty());
        let source = SourceBinding {
            repository_id: self.id,
            binding_generation: 1,
            logical_identity: "synthetic".into(),
            base: serde_json::from_value(oid).unwrap(),
        };
        let manifest = TransferManifest {
            transfer_id: artifact,
            artifact_id: artifact,
            role: SourceRole::Base,
            attempt: AttemptRef {
                attempt_id: Uuid::from_u128(1),
                session_id: Uuid::from_u128(1),
                generation: 1,
            },
            source: source.clone(),
            checkpoint: source.base,
            resolved_spec_digest: "a".repeat(64),
            manifest_bytes: manifest_bytes.len() as u64,
            manifest_digest: format!("{:x}", Sha256::digest(&manifest_bytes)),
            bundle_bytes: bundle.len() as u64,
            bundle_digest: Some(format!("{:x}", Sha256::digest(&bundle))),
        };
        Export {
            manifest,
            manifest_bytes,
            bundle,
        }
    }
}
