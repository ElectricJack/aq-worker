//! Actual HTTP ranges with synthetic bytes; aq-git import is a separate owner.
use aq_admission::{LaunchBinding, ObjectFormat, ObjectId, SourceAdmission, SourceSelection};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use aq_worker::{config::WorkerConfig, native::http::NativeClient};
use aq_worker_protocol::{AttemptRef, SourceBinding, SourceRole, TransferFile, TransferManifest};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;
const MANIFEST: &[u8] = b"synthetic retained manifest";
const BUNDLE: &[u8] = b"synthetic source bundle bytes";
#[derive(Clone)]
struct Data {
    manifest: TransferManifest,
    mode: Arc<AtomicUsize>,
}
struct Server {
    client: NativeClient,
    data: Data,
    binding: LaunchBinding,
    source: SourceAdmission,
    task: tokio::task::JoinHandle<()>,
}
fn headers(response: &mut Response, mode: usize) {
    response.headers_mut().insert(
        "aq-source-api-version",
        if mode == 4 { "2" } else { "1" }.parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
}
fn auth(h: &HeaderMap) {
    assert_eq!(h["authorization"], "Bearer synthetic-node");
    assert_eq!(h["aq-source-api-version"], "1");
}
async fn status(State(d): State<Data>, Path(attempt): Path<Uuid>, h: HeaderMap) -> Response {
    auth(&h);
    assert_eq!(attempt, d.manifest.attempt.attempt_id);
    let mode = d.mode.load(Ordering::SeqCst);
    let mut v = serde_json::to_value(&d.manifest).unwrap();
    match mode {
        1 => {
            v["source"]["base"]["hex"] = json!("b".repeat(40));
            v["checkpoint"]["hex"] = json!("b".repeat(40));
        }
        2 => v["artifact_id"] = json!(Uuid::new_v4()),
        3 => v["source"]["logical_identity"] = json!("replacement-transfer"),
        9 => v["attempt"]["attempt_id"] = json!(Uuid::new_v4()),
        10 => v["unknown"] = json!(true),
        _ => {}
    }
    let mut response = Json(v).into_response();
    headers(&mut response, mode);
    response
}
#[derive(Deserialize)]
struct Range {
    offset: u64,
    limit: u32,
}
async fn chunk(
    State(d): State<Data>,
    Path((attempt, member)): Path<(Uuid, String)>,
    Query(range): Query<Range>,
    h: HeaderMap,
) -> Response {
    auth(&h);
    assert_eq!(attempt, d.manifest.attempt.attempt_id);
    let mode = d.mode.load(Ordering::SeqCst);
    if mode == 8 {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/never-follow")],
        )
            .into_response();
    }
    let original = match member.as_str() {
        "manifest" => MANIFEST,
        "bundle" => BUNDLE,
        _ => panic!("unknown source member"),
    };
    let end = (range.offset as usize + range.limit as usize).min(original.len());
    let mut bytes = original[range.offset as usize..end].to_vec();
    if mode == 6 {
        bytes.pop();
    }
    let mut manifest = d.manifest.clone();
    if mode == 3 {
        manifest.source.logical_identity = "changed".into();
    }
    let metadata = if mode == 7 {
        "x".repeat(4097)
    } else {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&manifest).unwrap())
    };
    let mut response = bytes.into_response();
    headers(&mut response, mode);
    response
        .headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    response
        .headers_mut()
        .insert("aq-source-transfer", metadata.parse().unwrap());
    response.headers_mut().insert(
        "aq-source-offset",
        (if mode == 5 {
            range.offset + 1
        } else {
            range.offset
        })
        .to_string()
        .parse()
        .unwrap(),
    );
    response
}
impl Server {
    async fn new() -> Self {
        let f: Value = serde_json::from_slice(include_bytes!(
            "../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
        ))
        .unwrap();
        let binding: LaunchBinding =
            serde_json::from_value(f["requests"]["accept"]["acceptance"]["binding"].clone())
                .unwrap();
        let object = ObjectId {
            format: ObjectFormat::Sha1,
            hex: "a".repeat(40),
        };
        let source = SourceAdmission {
            schema_version: 1,
            project_id: Uuid::new_v4(),
            project_revision: 1,
            repository_id: Uuid::new_v4(),
            repository_binding_generation: 1,
            policy: ArtifactRef {
                id: Text::new("synthetic-policy").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"synthetic"),
            },
            selection: SourceSelection::Immutable {
                object: object.clone(),
            },
            resolved_object: object,
            observed_at_ms: 1000,
            expires_at_ms: 2000,
        };
        let aq_session_protocol::SessionIdentity::Remote {
            node_id,
            session_id,
            ..
        } = binding.session
        else {
            panic!()
        };
        let object = aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: source.resolved_object.hex.clone(),
        };
        let id = Uuid::new_v4();
        let manifest = TransferManifest {
            transfer_id: id,
            artifact_id: id,
            role: SourceRole::Base,
            checkpoint: object.clone(),
            attempt: AttemptRef {
                attempt_id: binding.attempt_id,
                session_id,
                generation: binding.assignment_generation,
            },
            resolved_spec_digest: binding.execution_spec_digest.as_str().into(),
            source: SourceBinding {
                repository_id: source.repository_id,
                binding_generation: 1,
                logical_identity: "synthetic".into(),
                base: object,
            },
            manifest_digest: Digest::of(MANIFEST).as_str().into(),
            bundle_digest: Some(Digest::of(BUNDLE).as_str().into()),
            manifest_bytes: MANIFEST.len() as u64,
            bundle_bytes: BUNDLE.len() as u64,
        };
        let data = Data {
            manifest,
            mode: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = NativeClient::from_config(&WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id,
            credential: "synthetic-node".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let app = Router::new()
            .route("/v1/worker/profiled/source/{attempt}", get(status))
            .route("/v1/worker/profiled/source/{attempt}/{member}", get(chunk))
            .with_state(data.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            client,
            data,
            binding,
            source,
            task,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn source_manifest_pins_and_bounded_member_ranges_are_exact() {
    let s = Server::new().await;
    let manifest = s
        .client
        .source_status(&s.binding, &s.source, None)
        .await
        .unwrap();
    assert_eq!(manifest, s.data.manifest);
    assert_eq!(
        s.client
            .source_status(&s.binding, &s.source, Some(&manifest))
            .await
            .unwrap(),
        manifest
    );
    for (member, expected) in [
        (TransferFile::Manifest, MANIFEST),
        (TransferFile::Bundle, BUNDLE),
    ] {
        let mut all = Vec::new();
        while all.len() < expected.len() {
            let chunk = s
                .client
                .source_chunk(&s.binding, &manifest, member.clone(), all.len() as u64, 7)
                .await
                .unwrap();
            assert_eq!(chunk.offset, all.len() as u64);
            assert_eq!(chunk.manifest, manifest);
            all.extend(chunk.bytes);
        }
        assert_eq!(all, expected);
    }
    assert!(
        s.client
            .source_chunk(&s.binding, &manifest, TransferFile::Bundle, 0, 262145)
            .await
            .is_err()
    );
    assert!(
        s.client
            .source_chunk(
                &s.binding,
                &manifest,
                TransferFile::Bundle,
                manifest.bundle_bytes,
                1
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn source_scope_status_replay_and_protocol_variants_refuse() {
    let s = Server::new().await;
    for mode in [1, 2, 3, 4, 9, 10] {
        s.data.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client
                .source_status(&s.binding, &s.source, Some(&s.data.manifest))
                .await
                .is_err(),
            "mode{mode}"
        );
    }
    s.data.mode.store(0, Ordering::SeqCst);
    let mut other = s.source.clone();
    other.repository_binding_generation += 1;
    assert!(
        s.client
            .source_status(&s.binding, &other, None)
            .await
            .is_err()
    );
    let mut empty = s.data.manifest.clone();
    empty.bundle_bytes = 0;
    empty.bundle_digest = None;
    assert!(
        s.client
            .source_chunk(&s.binding, &empty, TransferFile::Manifest, 0, 1)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn chunk_metadata_offset_size_and_redirects_are_fail_closed() {
    let s = Server::new().await;
    for mode in [3, 4, 5, 6, 7, 8] {
        s.data.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client
                .source_chunk(&s.binding, &s.data.manifest, TransferFile::Bundle, 0, 8)
                .await
                .is_err(),
            "mode{mode}"
        );
    }
}
