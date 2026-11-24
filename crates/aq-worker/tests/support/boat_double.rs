//! Loopback HTTP double; faults can happen before or after a real side effect.
use axum::{
    Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct FaultRule {
    pub method: Method,
    pub path: String,
    pub status: u16,
    pub after: bool,
}
#[derive(Default)]
pub struct Data {
    pub sequence: usize,
    pub sandboxes: BTreeMap<String, Value>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub requests: Vec<(Method, String, Value)>,
    pub faults: Vec<FaultRule>,
    pub lie_stop: bool,
    pub lie_delete: bool,
    pub lie_purge: bool,
    pub snapshot_left: bool,
    pub page: bool,
    pub snapshot_id: String,
    /// Personal account (no organization) with the stock image: no `team`,
    /// `access == "owner"`, no `from`, and no org-only `visibility`.
    pub personal: bool,
}
pub struct BoatDouble {
    pub url: String,
    pub data: Arc<Mutex<Data>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for BoatDouble {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl BoatDouble {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let data = Arc::new(Mutex::new(Data {
            snapshot_id: "template-artifact".into(),
            ..Default::default()
        }));
        let app = Router::new().fallback(any(handle)).with_state(data.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, data, task }
    }
    pub fn fail_before(&self, method: Method, path: &str, status: u16) {
        self.queue_fault(FaultRule {
            method,
            path: path.into(),
            status,
            after: false,
        });
    }
    pub fn fail_after(&self, method: Method, path: &str, status: u16) {
        self.queue_fault(FaultRule {
            method,
            path: path.into(),
            status,
            after: true,
        });
    }
    fn queue_fault(&self, rule: FaultRule) {
        self.data.lock().unwrap().faults.push(rule);
    }
    pub async fn start_personal() -> Self {
        let d = Self::start().await;
        d.data.lock().unwrap().personal = true;
        d
    }
    /// Raw provider object, for scope fields beyond an org id.
    pub fn insert_raw(&self, id: &str, value: Value) {
        self.data.lock().unwrap().sandboxes.insert(id.into(), value);
    }
    pub fn insert(&self, id: &str, name: &str, org: &str) {
        self.data.lock().unwrap().sandboxes.insert(id.into(),json!({"id":id,"name":name,"state":"running","team":{"id":org},"snapshotAvailable":false}));
    }
    pub fn running(&self) -> usize {
        self.data
            .lock()
            .unwrap()
            .sandboxes
            .values()
            .filter(|s| s["state"] != "archived")
            .count()
    }
}
fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
}
async fn handle(
    State(data): State<Arc<Mutex<Data>>>,
    OriginalUri(uri): OriginalUri,
    request: axum::http::request::Parts,
    bytes: Bytes,
) -> Response {
    let headers = &request.headers;
    let method = &request.method;
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer fixture-node-key"
    );
    let path = uri.path().strip_prefix("/api/v1").unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut d = data.lock().unwrap();
    let personal = d.personal;
    assert_eq!(
        headers.get("x-boat-org").unwrap(),
        if personal {
            "personal"
        } else {
            "team_attempts"
        }
    );
    d.requests
        .push((method.clone(), uri.to_string(), body.clone()));
    let rule = d
        .faults
        .iter()
        .position(|r| r.method == *method && r.path == path)
        .map(|i| d.faults.remove(i));
    if let Some(r) = rule.as_ref().filter(|r| !r.after) {
        return fault_status(r.status);
    }
    let route = BoatRequest {
        path,
        query: uri.query(),
        method,
        headers,
        body: &body,
    };
    let response = d.route(&route);
    match rule {
        Some(r) if r.status == 0 => (StatusCode::OK, "lost reply").into_response(),
        Some(r) => fault_status(r.status),
        None => response,
    }
}
fn fault_status(status: u16) -> Response {
    let code = if status == 409 {
        "sandbox_limit_reached"
    } else {
        "unavailable"
    };
    reply(status, json!({"ok":false,"code":code}))
}

struct BoatRequest<'a> {
    path: &'a str,
    query: Option<&'a str>,
    method: &'a Method,
    headers: &'a HeaderMap,
    body: &'a Value,
}
impl Data {
    fn route(&mut self, req: &BoatRequest) -> Response {
        if req.path.starts_with("/saved-snapshots/") {
            assert!(
                !self.personal,
                "stock template must not read a saved snapshot"
            );
            return reply(
                200,
                json!({"ok":true,"snapshot":{"name":"fixture-template","status":"ready","snapshotId":self.snapshot_id}}),
            );
        }
        if req.path == "/sandboxes" {
            return if *req.method == Method::POST {
                self.create(req)
            } else {
                self.list(req.query)
            };
        }
        if let Some(id) = req.path.strip_prefix("/deletion-operations/delete-") {
            return self.delete_operation(id);
        }
        if req.path.starts_with("/snapshots/") && req.path.ends_with("/files") {
            return self.snapshot_file(req);
        }
        self.sandbox_route(req)
    }

    fn create(&mut self, req: &BoatRequest) -> Response {
        let body = req.body;
        let headers = req.headers;
        assert!(headers.get("idempotency-key").is_some());
        assert_eq!(body["noEnv"], true);
        assert_eq!(body["env"], json!({}));
        if self.personal {
            assert!(body.get("from").is_none());
            assert!(body.get("visibility").is_none());
        } else {
            assert_eq!(body["from"], "fixture-template");
            assert_eq!(body["visibility"], "private");
        }
        assert!(body.get("hostedPorts").is_none());
        assert!(body["ttlSeconds"].as_u64().unwrap() > 0);
        self.sequence += 1;
        let id = format!("bx_{:08}", self.sequence);
        let s = if self.personal {
            json!({"id":id,"name":body["name"],"state":"ready","team":null,"visibility":null,"access":"owner","snapshotAvailable":false})
        } else {
            json!({"id":id,"name":body["name"],"state":"ready","team":{"id":"team_attempts"},"snapshotAvailable":false})
        };
        let mut s = s;
        s["createdAt"] = json!("2026-01-01T00:00:00Z");
        self.sandboxes.insert(id, s.clone());
        reply(202, json!({"ok":true,"sandbox":s}))
    }

    fn list(&mut self, query: Option<&str>) -> Response {
        let all: Vec<_> = self.sandboxes.values().cloned().collect();
        let paged = self.page;
        let cursor = query.unwrap_or("").contains("cursor=");
        let (items, next) = match (paged, cursor, all.is_empty()) {
            (true, false, false) => (all[..1].to_vec(), Some("next")),
            (true, true, _) => (all[1..].to_vec(), None),
            _ => (all, None),
        };
        // Deliberately ignore the server-side name filter: the adapter must check too.
        reply(
            200,
            json!({"ok":true,"sandboxes":items,"pageInfo":{"nextCursor":next,"hasMore":next.is_some()}}),
        )
    }

    fn delete_operation(&mut self, id: &str) -> Response {
        if !self.lie_delete && !self.lie_purge {
            self.sandboxes.remove(id);
        }
        reply(
            200,
            json!({"ok":true,"operation":{"id":format!("delete-{id}"),"kind":"sandbox","targetId":id,"status":if self.lie_purge {"processing"}else{"completed"}}}),
        )
    }

    fn snapshot_file(&mut self, req: &BoatRequest) -> Response {
        // Real Boat serves Docker volume contents from the snapshot root at
        // `__dockervol__/<volume>/_data/<path>`; any other path is a 404.
        let id = req
            .path
            .trim_start_matches("/snapshots/snapshot-")
            .trim_end_matches("/files");
        let name = self.sandboxes.get(id).and_then(|s| s["name"].as_str());
        let wanted = name.map(|n| format!("path=__dockervol__%2F{n}-work%2F_data%2Fresult"));
        if wanted.is_some_and(|w| req.query == Some(w.as_str())) {
            (StatusCode::OK, b"fixture-complete\n".to_vec()).into_response()
        } else {
            reply(404, json!({"ok":false,"error":{"code":"path_not_found"}}))
        }
    }

    fn sandbox_route(&mut self, req: &BoatRequest) -> Response {
        let parts: Vec<_> = req.path.trim_matches('/').split('/').collect();
        let id = parts.get(1).copied().unwrap_or("");
        let action = parts.get(2).copied();
        if parts.first() != Some(&"sandboxes") {
            return reply(404, json!({"ok":false}));
        }
        match (action, req.method) {
            (None, &Method::GET) if parts.len() == 2 => match self.sandboxes.get(id) {
                Some(s) => reply(200, json!({"ok":true,"sandbox":s})),
                None => reply(404, json!({"ok":false})),
            },
            (None, &Method::DELETE) if parts.len() == 2 => self.delete_sandbox(id, req.headers),
            (None, &Method::PATCH) if parts.len() == 2 => reply(200, json!({"ok":true})),
            (Some("stop"), _) => self.stop_sandbox(id),
            (Some("snapshots"), _) => self.snapshot_response(id, req, parts.len()),
            (Some("files"), _) => {
                let content = STANDARD
                    .decode(req.body["content"].as_str().unwrap())
                    .unwrap();
                self.files.insert(id.into(), content);
                reply(200, json!({"ok":true}))
            }
            (Some("commands"), _) => Self::command_reply(req.body),
            _ => reply(404, json!({"ok":false})),
        }
    }

    fn snapshot_response(&self, id: &str, req: &BoatRequest, count: usize) -> Response {
        let headers = req.headers;
        match (req.method, count) {
            (&Method::DELETE, _) => {
                assert_eq!(headers.get("x-ascii-confirm-delete").unwrap(), id);
                reply(202, json!({"ok":true,"operations":[]}))
            }
            (_, 4) => reply(
                200,
                json!({"ok":true,"snapshot":{"id":format!("snapshot-{id}")}}),
            ),
            _ => reply(
                200,
                json!({"ok":true,"snapshots":if self.snapshot_left {vec![json!({"id":"leftover"})]}else{vec![]}}),
            ),
        }
    }
    fn delete_sandbox(&mut self, id: &str, headers: &HeaderMap) -> Response {
        assert_eq!(headers.get("x-ascii-confirm-delete").unwrap(), id);
        if !self.lie_delete {
            self.sandboxes.remove(id);
        }
        reply(
            202,
            json!({"ok":true,"operation":{"id":format!("delete-{id}"),"kind":"sandbox","targetId":id,"status":"pending"}}),
        )
    }

    fn stop_sandbox(&mut self, id: &str) -> Response {
        if !self.lie_stop
            && let Some(s) = self.sandboxes.get_mut(id)
        {
            s["state"] = json!("archived");
            s["snapshotAvailable"] = json!(true);
            s["stop"] = json!({"status":"completed"});
        }
        reply(202, json!({"ok":true}))
    }

    fn command_reply(body: &Value) -> Response {
        let command = body["command"].as_str().unwrap();
        assert!(!command.contains("fixture-node-key"));
        assert!(!command.contains("fixture-private-material"));
        let stdout = if command.contains("base64 -w0") {
            STANDARD.encode(b"fixture-complete\n")
        } else {
            String::new()
        };
        reply(
            200,
            json!({"ok":true,"success":true,"exitCode":0,"stdout":stdout,"stderr":"","timedOut":false}),
        )
    }
}
