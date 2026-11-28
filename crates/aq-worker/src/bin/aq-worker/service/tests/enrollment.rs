use super::*;
/// A loopback stand-in for aq-server's enroll and slots routes. The slots route answers
/// 409 for the first `refuse_claims` calls.
pub(super) struct FakeServer {
    pub(super) origin: String,
    pub(super) enrolls: Arc<std::sync::atomic::AtomicU32>,
    pub(super) claims: Arc<std::sync::atomic::AtomicU32>,
    _server: tokio::task::AbortHandle,
}
pub(super) async fn fake_server(refuse_claims: u32) -> FakeServer {
    fake_server_with(refuse_claims, None).await
}
/// Like aq-server: a slot's credential is returned only by the claim that minted it (one
/// per slot-1 credential here). With `access`, the slots route sits behind an edge that
/// answers a bare 403 unless that Access client id is sent.
pub(super) async fn fake_server_with(
    refuse_claims: u32,
    access: Option<&'static str>,
) -> FakeServer {
    use axum::{
        Json, Router, http::HeaderMap, http::StatusCode, response::IntoResponse, routing::post,
    };
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    let (enrolls, claims) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let (e, c) = (enrolls.clone(), claims.clone());
    let minted = Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<String>::new(),
    ));
    let app = Router::new()
        .route(
            "/v1/worker/enroll",
            post(move || async move {
                let n = e.fetch_add(1, SeqCst) + 1;
                Json(serde_json::json!({"node_id": Uuid::from_u128(n.into()),
                "credential": format!("aqn_cred{n}"), "heartbeat_interval_secs": 30}))
            }),
        )
        .route(
            "/v1/worker/slots",
            post(move |h: HeaderMap| async move {
                let header = |k: &str| {
                    h.get(k)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned()
                };
                if let Some(id) = access
                    && header("cf-access-client-id") != id
                {
                    return (StatusCode::FORBIDDEN, "<html>Forbidden</html>").into_response();
                }
                if c.fetch_add(1, SeqCst) < refuse_claims {
                    let body = serde_json::json!({"error": "conflict", "message": "in use"});
                    return (StatusCode::CONFLICT, Json(body)).into_response();
                }
                let new = minted.lock().unwrap().insert(header("authorization"));
                let credential = new.then_some("aqn_slot2");
                let body = serde_json::json!({"schema_version": 1, "machine_key": "desk", "slots": [
                {"slot": 2, "node_id": Uuid::from_u128(99), "credential": credential}]});
                (StatusCode::OK, Json(body)).into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeServer {
        origin,
        enrolls,
        claims,
        _server: server.abort_handle(),
    }
}
pub(super) struct Profile {
    pub(super) dir: tempfile::TempDir,
    pub(super) layout: Layout,
}
pub(super) fn profile() -> Profile {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("native.json"), b"{}").unwrap();
    std::fs::write(dir.path().join("aq-git"), b"").unwrap();
    let layout = Layout {
        node: None,
        config_dir: dir.path().join("cfg"),
        state_root: dir.path().join("state"),
        runtime_root: dir.path().join("run/local"),
    };
    Profile { dir, layout }
}
impl Profile {
    pub(super) fn args(&self, server: &str) -> ServiceEnrollArgs {
        let at = |name: &str| self.dir.path().join(name);
        ServiceEnrollArgs {
            profile: "local".into(),
            server: server.into(),
            nodes: 2,
            name: Some("Desk".into()),
            machine_key: Some("desk".into()),
            cf_service_token: false,
            native_template: at("native.json"),
            aq_git: at("aq-git"),
            git_executable: "/usr/bin/git".into(),
            reconfigure: false,
        }
    }
    pub(super) async fn enroll(&self, args: &ServiceEnrollArgs, code: Option<&str>) -> Result<()> {
        let code = code.map(str::to_owned);
        let secrets = move |wanted: bool| {
            assert!(wanted, "only the code is typed here");
            let code = code.expect("no secret may be read here");
            Ok(Enrollment { code, cf: None })
        };
        self.enroll_with(args, secrets).await
    }
    /// A resumed enrollment that types only new Access credentials.
    pub(super) async fn enroll_access(&self, args: &ServiceEnrollArgs, id: &str) -> Result<()> {
        let id = id.to_owned();
        let secrets = move |wanted: bool| {
            assert!(!wanted, "a resume needs no code");
            Ok(Enrollment {
                code: String::new(),
                cf: Some((id, "new-secret".into())),
            })
        };
        self.enroll_with(args, secrets).await
    }
    async fn enroll_with(
        &self,
        args: &ServiceEnrollArgs,
        secrets: impl FnOnce(bool) -> Result<Enrollment>,
    ) -> Result<()> {
        enroll_in(
            args,
            &self.layout,
            Host {
                name: Some("desk".into()),
                wsl: false,
            },
            secrets,
        )
        .await
    }
    /// Slot 1's redeemed credential, saved before the claim (as a cut run leaves it).
    pub(super) fn pending(&self, server: &str) -> EnrollmentFile {
        let first = EnrollmentFile {
            server_url: server.into(),
            node_id: Uuid::from_u128(1),
            credential: "aqn_cred1".into(),
            cf_client_id: Some("old-id".into()),
            cf_client_secret: Some("old-secret".into()),
        };
        first.save(&pending_enrollment(&self.layout)).unwrap();
        first
    }
    pub(super) fn slot1(&self) -> String {
        std::fs::read_to_string(self.layout.slot_enrollment(1)).unwrap_or_default()
    }
}
pub(super) fn count(n: &std::sync::atomic::AtomicU32) -> u32 {
    n.load(std::sync::atomic::Ordering::SeqCst)
}
