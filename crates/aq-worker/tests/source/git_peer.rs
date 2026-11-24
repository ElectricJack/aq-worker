struct Peer {
    _temp: TempDir,
    client: GitClient,
    requests: Arc<Mutex<Vec<Value>>>,
}
fn peer(lose_create: bool) -> Peer {
    let temp = TempDir::new().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = temp.path().join("git.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let work = temp
        .path()
        .join("workspaces")
        .join(Uuid::from_u128(70).to_string());
    std::fs::create_dir_all(&work).unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let recorded = requests.clone();
    let artifact_root = temp.path().to_path_buf();
    std::thread::spawn(move || {
        let mut lose_next_create = lose_create;
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut line = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            recorded.lock().unwrap().push(req.clone());
            if req["operation"] == "workspace.create" && lose_next_create {
                lose_next_create = false;
                continue;
            }
            let result = git_reply(&req, &work, &artifact_root);
            let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true,"result":result});
            if let Some(id) = req.get("operation_id") {
                reply["operation_id"] = id.clone();
            }
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            let _ = conn.write_all(&bytes);
        }
    });
    let client = GitClient::new(&socket, temp.path()).unwrap();
    Peer {
        _temp: temp,
        client,
        requests,
    }
}
fn git_reply(req: &Value, work: &Path, artifact_root: &Path) -> Value {
    match req["operation"].as_str().unwrap() {
        "capabilities" => {
            json!({"protocol_version":1,"artifact_format_version":1,"limits":{"artifact":{"max_expanded_bytes":536870912_u64,"max_bundle_bytes":67108864_u64}},"git":{"checkout_profile":"no_attributes_v1"},"operations":["repo.show","artifact.import","workspace.create","workspace.checkpoint","workspace.retire","artifact.export","retention.release","repo.cache-evict"]})
        }
        "repo.show" => {
            json!({"repository_id":Uuid::from_u128(50),"binding_generation":1,"identity":"synthetic","mode":"worker"})
        }
        "workspace.create" => {
            json!({"workspace_id":Uuid::from_u128(70),"generation":1,"path":work})
        }
        "workspace.inspect" => json!({"head":{"algorithm":"sha1","hex":"a".repeat(40)}}),
        "workspace.checkpoint" => {
            json!({"workspace_id":Uuid::from_u128(70),"workspace_generation":2,"source":{"algorithm":"sha1","hex":"b".repeat(40)},"retention_handle":Uuid::from_u128(80)})
        }
        "workspace.retire" => json!({"retired":true}),
        "artifact.export" => export_artifact(artifact_root),
        other => panic!("unexpected operation {other}"),
    }
}

fn export_artifact(artifact_root: &Path) -> Value {
    use sha2::{Digest, Sha256};
    let artifact = Uuid::from_u128(81);
    let bundle = vec![b'q'; 600_000];
    let manifest = json!({
        "format_version":1,"repository_id":Uuid::from_u128(50),
        "repository_identity":"synthetic","binding_generation":1,
        "transfer_id":artifact,"object_format":"sha1",
        "base":{"algorithm":"sha1","hex":"a".repeat(40)},
        "source":{"algorithm":"sha1","hex":"b".repeat(40)},
        "prerequisites":[],"bundle_size":bundle.len(),
        "bundle_sha256":format!("{:x}",Sha256::digest(&bundle)),
        "producer_installation_id":Uuid::from_u128(82),
        "reference_only":false,"caller_correlation":null
    });
    let directory = artifact_root.join("artifacts").join(artifact.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.join("objects.bundle"), bundle).unwrap();
    json!({"artifact_handle":artifact,"retention_handle":artifact,"manifest":manifest})
}
