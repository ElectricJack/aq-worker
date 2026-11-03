//! Runner hydration through the retained source export and a real-Git aq-git peer.
use super::auth_tests::client;
use super::hydrate;
use crate::source::{GitClient, GitManifest};
use aq_worker_protocol::{
    AttemptRef, ObjectAlgorithm, ObjectId, SourceBinding, SourceRole, TransferManifest,
};
use axum::{
    Router,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const ATTEMPT: &str = "00000000-0000-0000-0000-000000000001";
const BEARER: &str = "Bearer fixture_only_not_a_real_secret_00000000";

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A retained revision exactly as central retention describes it.
struct Retained {
    source: SourceBinding,
    transfer: TransferManifest,
    manifest: Vec<u8>,
    bundle: Vec<u8>,
    commit: String,
}
fn retained(tmp: &Path) -> Retained {
    let home = tmp.join("home");
    let repo = tmp.join("origin");
    private_dir(&home);
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    // The project remote is unreachable on purpose; hydration must not use it.
    git(
        &repo,
        &home,
        &["remote", "add", "origin", "https://git.invalid/project.git"],
    );
    std::fs::write(repo.join("README.md"), "retained revision\n").unwrap();
    git(&repo, &home, &["add", "README.md"]);
    git(
        &repo,
        &home,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@invalid",
            "commit",
            "-q",
            "-m",
            "retained",
        ],
    );
    let commit = git(&repo, &home, &["rev-parse", "HEAD"]);
    let bundle_path = tmp.join("objects.bundle");
    git(
        &repo,
        &home,
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    );
    let bundle = std::fs::read(&bundle_path).unwrap();
    let id = Uuid::new_v4();
    let base = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: commit.clone(),
    };
    let source = SourceBinding {
        repository_id: Uuid::new_v4(),
        binding_generation: 1,
        logical_identity: "fixture/project".into(),
        base: base.clone(),
    };
    let digest = format!("{:x}", Sha256::digest(&bundle));
    let native = GitManifest {
        format_version: 1,
        repository_id: source.repository_id,
        repository_identity: source.logical_identity.clone(),
        binding_generation: 1,
        transfer_id: id,
        object_format: "sha1".into(),
        base: base.clone(),
        source: base.clone(),
        prerequisites: vec![],
        bundle_size: bundle.len() as u64,
        bundle_sha256: Some(digest.clone()),
        producer_installation_id: Uuid::new_v4(),
        reference_only: false,
        caller_correlation: json!({}),
    };
    let manifest = serde_json::to_vec(&native).unwrap();
    // Association with the fixture permit's execution LaunchBinding.
    let transfer = TransferManifest {
        transfer_id: id,
        artifact_id: id,
        role: SourceRole::Base,
        attempt: AttemptRef {
            attempt_id: Uuid::parse_str(ATTEMPT).unwrap(),
            session_id: Uuid::parse_str("00000000-0000-0000-0000-000000000030").unwrap(),
            generation: 2,
        },
        source: source.clone(),
        checkpoint: base,
        resolved_spec_digest: "b".repeat(64),
        manifest_bytes: manifest.len() as u64,
        manifest_digest: format!("{:x}", Sha256::digest(&manifest)),
        bundle_bytes: bundle.len() as u64,
        bundle_digest: Some(digest),
    };
    Retained {
        source,
        transfer,
        manifest,
        bundle,
        commit,
    }
}

type Seen = Arc<Mutex<Vec<(String, Option<HeaderValue>)>>>;
struct Central {
    origin: String,
    seen: Seen,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Central {
    fn drop(&mut self) {
        self.task.abort();
    }
}
/// Central retention export: descriptor plus member ranges, permit-authenticated.
async fn central(r: &Retained, bundle: Vec<u8>) -> Central {
    let seen = Seen::default();
    let transfer = r.transfer.clone();
    let manifest = r.manifest.clone();
    let record = seen.clone();
    let handler = move |uri: Uri, headers: HeaderMap| {
        let (transfer, manifest, bundle, record) = (
            transfer.clone(),
            manifest.clone(),
            bundle.clone(),
            record.clone(),
        );
        async move {
            record
                .lock()
                .unwrap()
                .push((uri.to_string(), headers.get(header::AUTHORIZATION).cloned()));
            if headers
                .get(header::AUTHORIZATION)
                .is_none_or(|v| v != BEARER)
            {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let base = format!("/v1/worker/profiled/source/{ATTEMPT}");
            let common = [
                ("aq-source-api-version", "1"),
                ("cache-control", "no-store"),
            ];
            if uri.path() == base {
                let mut response: Response = serde_json::to_vec(&transfer).unwrap().into_response();
                for (k, v) in common {
                    response
                        .headers_mut()
                        .insert(k, HeaderValue::from_static(v));
                }
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                return response;
            }
            let member = match uri.path().strip_prefix(&format!("{base}/")) {
                Some("manifest") => manifest,
                Some("bundle") => bundle,
                _ => return StatusCode::NOT_FOUND.into_response(),
            };
            let query: std::collections::HashMap<&str, usize> = uri
                .query()
                .unwrap_or_default()
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k, v.parse().unwrap()))
                .collect();
            let (offset, limit) = (query["offset"], query["limit"]);
            let end = member.len().min(offset + limit);
            let mut response: Response = member[offset..end].to_vec().into_response();
            for (k, v) in common {
                response
                    .headers_mut()
                    .insert(k, HeaderValue::from_static(v));
            }
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(
                "aq-source-transfer",
                HeaderValue::from_str(
                    &URL_SAFE_NO_PAD.encode(serde_json::to_vec(&transfer).unwrap()),
                )
                .unwrap(),
            );
            h.insert("aq-source-offset", HeaderValue::from(offset as u64));
            response
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(any(handler));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Central { origin, seen, task }
}

/// Minimal aq-git protocol peer backed by real Git. It only reads published
/// artifacts from its managed root; it has no remote configuration at all.
struct FakeGit {
    socket: PathBuf,
    root: PathBuf,
    operations: Arc<Mutex<Vec<String>>>,
}
fn fake_git(tmp: &Path) -> FakeGit {
    let state = tmp.join("aq-git");
    let root = state.join("root");
    private_dir(&state);
    private_dir(&root);
    let home = state.join("home");
    private_dir(&home);
    let socket = state.join("git.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let operations = Arc::new(Mutex::new(Vec::new()));
    let seen = operations.clone();
    let repo = state.join("repo.git");
    let managed = root.clone();
    std::thread::spawn(move || {
        let mut created = Value::Null;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let op = req["operation"].as_str().unwrap().to_owned();
            seen.lock().unwrap().push(op.clone());
            let repository = req["repository_id"].clone();
            let result = match op.as_str() {
                "capabilities" => json!({"protocol_version":1,"artifact_format_version":1,
                    "limits":{"artifact":{"max_expanded_bytes":536870912,"max_bundle_bytes":67108864}},
                    "git":{"checkout_profile":"no_attributes_v1"},
                    "operations":["repo.register","repo.show","artifact.import","workspace.create",
                        "workspace.checkpoint","workspace.retire","artifact.export","retention.release",
                        "repo.cache-evict","workspace.inspect","retention.inspect"]}),
                "repo.register" | "repo.show" => {
                    if op == "repo.register" {
                        git(
                            &state,
                            &home,
                            &["init", "-q", "--bare", repo.to_str().unwrap()],
                        );
                    }
                    if !repo.exists() {
                        let reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":false,
                            "error":{"code":"not_found","message":"unregistered"}});
                        writeln!(stream, "{reply}").unwrap();
                        continue;
                    }
                    let identity = if op == "repo.register" {
                        req["params"]["binding"]["identity"].clone()
                    } else {
                        json!("fixture/project")
                    };
                    json!({"repository_id":repository,"binding_generation":1,"identity":identity,"mode":"worker"})
                }
                "artifact.import" => {
                    let id = req["params"]["artifact_handle"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let dir = managed.join("artifacts/incoming").join(&id);
                    let native: GitManifest =
                        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap())
                            .unwrap();
                    let bundle = dir.join("objects.bundle");
                    git(
                        &repo,
                        &home,
                        &["bundle", "verify", "-q", bundle.to_str().unwrap()],
                    );
                    git(
                        &repo,
                        &home,
                        &[
                            "fetch",
                            "-q",
                            bundle.to_str().unwrap(),
                            &format!("refs/heads/main:refs/aq-git/imports/{id}/main"),
                        ],
                    );
                    json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":repository,
                        "binding_generation":1,"transfer_id":id,"source":native.source,
                        "retention_handle":id,"manifest_digest":native.canonical_digest().unwrap()})
                }
                "workspace.create" => {
                    let id = Uuid::new_v4();
                    let path = managed.join("workspaces").join(id.to_string());
                    let base = req["params"]["base"]["hex"].as_str().unwrap();
                    git(
                        &repo,
                        &home,
                        &[
                            "worktree",
                            "add",
                            "-q",
                            "--detach",
                            path.to_str().unwrap(),
                            base,
                        ],
                    );
                    created = json!({"workspace_id":id,"repository_id":repository,"binding_generation":1,
                        "generation":1,"base":base,"path":path,"lifecycle":"active",
                        "allocating_operation":req["operation_id"]});
                    created.clone()
                }
                "retention.inspect" => {
                    let id = req["params"]["handle_id"].as_str().unwrap();
                    let name = format!("refs/aq-git/imports/{id}/main");
                    let oid = git(&repo, &home, &["rev-parse", &name]);
                    json!({"id":id,"repository_id":repository,"binding_generation":1,"generation":1,
                        "state":"active","refs":[{"name":name,"oid":oid}]})
                }
                "workspace.inspect" => {
                    let path = PathBuf::from(created["path"].as_str().unwrap());
                    let head = git(&path, &home, &["rev-parse", "HEAD"]);
                    let clean = git(&path, &home, &["status", "--porcelain"]).is_empty();
                    json!({"workspace":created,"head":{"algorithm":"sha1","hex":head},"clean":clean})
                }
                other => panic!("unexpected aq-git operation {other}"),
            };
            let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true,
                "result":result,"operation_state":"complete"});
            if let Some(id) = req.get("operation_id") {
                reply["operation_id"] = id.clone();
            }
            writeln!(stream, "{reply}").unwrap();
        }
    });
    FakeGit {
        socket,
        root,
        operations,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrated_workspace_matches_retained_revision() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let tmp = tempfile::tempdir().unwrap();
    let r = retained(tmp.path());
    let central = central(&r, r.bundle.clone()).await;
    let aq_git = fake_git(tmp.path());
    let git = GitClient::new(&aq_git.socket, &aq_git.root).unwrap();
    let (_dir, http, _permit) = client(&central.origin, false);

    let hydrated = hydrate::hydrate(&http, &r.source, &git).await.unwrap();

    let path = hydrated.workspace().path.clone();
    assert!(
        path.starts_with(
            std::fs::canonicalize(&aq_git.root)
                .unwrap()
                .join("workspaces")
        )
    );
    let home = tmp.path().join("home");
    assert_eq!(git_out(&path, &home, &["rev-parse", "HEAD"]), r.commit);
    assert_eq!(
        std::fs::read_to_string(path.join("README.md")).unwrap(),
        "retained revision\n"
    );
    assert!(git_out(&path, &home, &["status", "--porcelain"]).is_empty());
    assert!(
        git_out(&path, &home, &["remote"]).is_empty(),
        "hydrated workspace must hold no Git remote"
    );
    assert_eq!(hydrated.manifest(), &r.transfer);
    let seen = central.seen.lock().unwrap();
    assert!(seen.len() >= 3, "status, manifest and bundle fetched");
    for (uri, authorization) in seen.iter() {
        assert!(uri.starts_with(&format!("/v1/worker/profiled/source/{ATTEMPT}")));
        assert_eq!(authorization.as_ref().unwrap(), BEARER);
    }
    assert_eq!(
        *aq_git.operations.lock().unwrap(),
        [
            "capabilities",
            "repo.show",
            "repo.register",
            "repo.show",
            "artifact.import",
            "workspace.create",
            "retention.inspect",
            "workspace.inspect"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydration_refuses_retained_bundle_digest_mismatch() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let tmp = tempfile::tempdir().unwrap();
    let r = retained(tmp.path());
    let mut tampered = r.bundle.clone();
    *tampered.last_mut().unwrap() ^= 1;
    let central = central(&r, tampered).await;
    let aq_git = fake_git(tmp.path());
    let git = GitClient::new(&aq_git.socket, &aq_git.root).unwrap();
    let (_dir, http, _permit) = client(&central.origin, false);
    let err = hydrate::hydrate(&http, &r.source, &git)
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("bundle digest"), "{err:#}");
    assert!(
        !aq_git
            .operations
            .lock()
            .unwrap()
            .iter()
            .any(|op| op == "artifact.import" || op == "workspace.create"),
        "unverified bytes must never reach aq-git"
    );
}

fn git_out(dir: &Path, home: &Path, args: &[&str]) -> String {
    git(dir, home, args)
}
