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
    pub fn fault(&self, method: Method, path: &str, status: u16, after: bool) {
        self.data.lock().unwrap().faults.push(FaultRule {
            method,
            path: path.into(),
            status,
            after,
        });
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
    method: Method,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
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
        .position(|r| r.method == method && r.path == path)
        .map(|i| d.faults.remove(i));
    if let Some(r) = &rule
        && !r.after
    {
        return reply(
            r.status,
            json!({"ok":false,"code":if r.status==409 {"sandbox_limit_reached"} else {"unavailable"}}),
        );
    }
    let response = if path.starts_with("/saved-snapshots/") {
        assert!(!personal, "stock template must not read a saved snapshot");
        reply(
            200,
            json!({"ok":true,"snapshot":{"name":"fixture-template","status":"ready","snapshotId":d.snapshot_id}}),
        )
    } else if path == "/sandboxes" && method == Method::POST {
        assert!(headers.get("idempotency-key").is_some());
        assert_eq!(body["noEnv"], true);
        assert_eq!(body["env"], json!({}));
        if personal {
            assert!(body.get("from").is_none());
            assert!(body.get("visibility").is_none());
        } else {
            assert_eq!(body["from"], "fixture-template");
            assert_eq!(body["visibility"], "private");
        }
        assert!(body.get("hostedPorts").is_none());
        assert!(body["ttlSeconds"].as_u64().unwrap() > 0);
        d.sequence += 1;
        let id = format!("bx_{:08}", d.sequence);
        let s = if personal {
            json!({"id":id,"name":body["name"],"state":"ready","team":null,"visibility":null,"access":"owner","snapshotAvailable":false})
        } else {
            json!({"id":id,"name":body["name"],"state":"ready","team":{"id":"team_attempts"},"snapshotAvailable":false})
        };
        let mut s = s;
        s["createdAt"] = json!("2026-01-01T00:00:00Z");
        d.sandboxes.insert(id, s.clone());
        reply(202, json!({"ok":true,"sandbox":s}))
    } else if path == "/sandboxes" {
        let all: Vec<_> = d.sandboxes.values().cloned().collect();
        let paged = d.page;
        let cursor = uri.query().unwrap_or("").contains("cursor=");
        let (items, next) = if paged && !cursor && !all.is_empty() {
            (all[..1].to_vec(), Some("next"))
        } else if paged && cursor {
            (all[1..].to_vec(), None)
        } else {
            (all, None)
        };
        // Deliberately ignore the server-side name filter: the adapter must check too.
        reply(
            200,
            json!({"ok":true,"sandboxes":items,"pageInfo":{"nextCursor":next,"hasMore":next.is_some()}}),
        )
    } else if path.starts_with("/deletion-operations/delete-") {
        let id = path.strip_prefix("/deletion-operations/delete-").unwrap();
        if !d.lie_delete && !d.lie_purge {
            d.sandboxes.remove(id);
        }
        reply(
            200,
            json!({"ok":true,"operation":{"id":format!("delete-{id}"),"kind":"sandbox","targetId":id,"status":if d.lie_purge {"processing"}else{"completed"}}}),
        )
    } else if path.starts_with("/snapshots/") && path.ends_with("/files") {
        (StatusCode::OK, b"fixture-complete\n".to_vec()).into_response()
    } else {
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        let id = parts.get(1).copied().unwrap_or("");
        if parts.first() != Some(&"sandboxes") {
            reply(404, json!({"ok":false}))
        } else if parts.len() == 2 && method == Method::GET {
            match d.sandboxes.get(id) {
                Some(s) => reply(200, json!({"ok":true,"sandbox":s})),
                None => reply(404, json!({"ok":false})),
            }
        } else if parts.len() == 2 && method == Method::DELETE {
            assert_eq!(headers.get("x-ascii-confirm-delete").unwrap(), id);
            if !d.lie_delete {
                d.sandboxes.remove(id);
            }
            reply(
                202,
                json!({"ok":true,"operation":{"id":format!("delete-{id}"),"kind":"sandbox","targetId":id,"status":"pending"}}),
            )
        } else if parts.get(2) == Some(&"stop") {
            if !d.lie_stop
                && let Some(s) = d.sandboxes.get_mut(id)
            {
                s["state"] = json!("archived");
                s["snapshotAvailable"] = json!(true);
                s["stop"] = json!({"status":"completed"});
            }
            reply(202, json!({"ok":true}))
        } else if parts.get(2) == Some(&"snapshots") && method == Method::DELETE {
            assert_eq!(headers.get("x-ascii-confirm-delete").unwrap(), id);
            reply(202, json!({"ok":true,"operations":[]}))
        } else if parts.get(2) == Some(&"snapshots") && parts.len() == 4 {
            reply(
                200,
                json!({"ok":true,"snapshot":{"id":format!("snapshot-{id}")}}),
            )
        } else if parts.get(2) == Some(&"snapshots") {
            reply(
                200,
                json!({"ok":true,"snapshots":if d.snapshot_left {vec![json!({"id":"leftover"})]}else {vec![]}}),
            )
        } else if parts.get(2) == Some(&"files") {
            let content = STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
            d.files.insert(id.into(), content);
            reply(200, json!({"ok":true}))
        } else if parts.get(2) == Some(&"commands") {
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
        } else if parts.len() == 2 && method == Method::PATCH {
            reply(200, json!({"ok":true}))
        } else {
            reply(404, json!({"ok":false}))
        }
    };
    if let Some(r) = rule {
        if r.status == 0 {
            return (StatusCode::OK, "lost reply").into_response();
        }
        return reply(
            r.status,
            json!({"ok":false,"code":if r.status==409 {"sandbox_limit_reached"} else {"unavailable"}}),
        );
    }
    response
}
