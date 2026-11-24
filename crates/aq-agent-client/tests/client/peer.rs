use super::*;

/// The agent-comments v1 routes as aq-server serves them (protocol/agent-comments/v1).
#[allow(clippy::result_large_err)] // test fixture: the error is the reply
fn comment_route(s: &Peer, path: &str, raw: &[u8]) -> Result<Value, Response> {
    let request: Value = serde_json::from_slice(raw).unwrap();
    let binding = &s.base["context"]["binding"];
    if s.mode == 32 {
        return Err(agent_error(StatusCode::UNAUTHORIZED, "session_revoked"));
    }
    if path == "/v1/agent/comments" {
        return add_comment(s, &request, raw, binding);
    }
    assert_eq!(path, "/v1/agent/comments/list");
    Ok(list_comments(s, &request, binding))
}

#[allow(clippy::result_large_err)]
fn add_comment(s: &Peer, request: &Value, raw: &[u8], binding: &Value) -> Result<Value, Response> {
    let mut thread = s.comments.lock().unwrap();
    assert_eq!(
        request.as_object().unwrap().len(),
        3,
        "only api_version, operation_id and body: {request}"
    );
    s.posts.lock().unwrap().push(raw.to_vec());
    let op = &request["operation_id"];
    let comment = match thread.iter().find(|c| &c["comment_id"] == op) {
        Some(c) if c["body"] == request["body"] => c.clone(),
        Some(_) => return Err(agent_error(StatusCode::CONFLICT, "id_conflict")),
        None => {
            let c = json!({"comment_id":op,"task_id":binding["task_id"],
                    "author":{"kind":"agent","user_id":null,"display_name":null,
                        "session_id":binding["session_id"],"attempt_id":binding["attempt_id"]},
                    "body":request["body"],"created_at":format!("2026-11-06T12:00:{:02}.000Z", thread.len()),
                    "edited_at":null,"deleted_at":null,"revision":"1","can_edit":false});
            thread.push(c.clone());
            c
        }
    };
    if s.mode == 31 && s.posts.lock().unwrap().len() == 1 {
        // Recorded, but the response is lost on the way back.
        return Err((
            StatusCode::BAD_GATEWAY,
            [("X-AQ-Agent-API-Version", "1")],
            "lost",
        )
            .into_response());
    }
    let mut comment = comment;
    if s.mode == 30 {
        comment["author"]["attempt_id"] = json!(Uuid::new_v4());
    }
    Ok(json!({"api_version":1,"comment":comment}))
}

fn list_comments(s: &Peer, request: &Value, binding: &Value) -> Value {
    let thread = s.comments.lock().unwrap();
    let limit = request["limit"].as_u64().unwrap_or(50) as usize;
    let before = request["cursor"]
        .as_str()
        .map(|c| c.strip_prefix("tc1.").unwrap().parse::<usize>().unwrap())
        .unwrap_or(thread.len() + 1);
    let mut items: Vec<(usize, Value)> = thread
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, c)| (i + 1, c))
        .filter(|(seq, _)| *seq < before)
        .rev()
        .collect();
    let more = items.len() > limit;
    items.truncate(limit);
    let next = more.then(|| format!("tc1.{}", items.last().unwrap().0));
    let mut items: Vec<Value> = items.into_iter().map(|(_, c)| c).collect();
    if s.mode == 33 {
        items[0]["task_id"] = json!(Uuid::new_v4());
    }
    json!({"api_version":1,"task_id":binding["task_id"],"items":items,"next_cursor":next})
}
pub(super) async fn endpoint(State(s): State<Arc<Peer>>, req: Request<Body>) -> Response {
    assert_eq!(
        req.headers()["authorization"],
        format!("Bearer aqa_{}", "q".repeat(40))
    );
    assert_eq!(req.headers()["X-AQ-Agent-API-Version"], "1");
    assert!(req.headers().get("cf-access-client-secret").is_none());
    s.calls.fetch_add(1, Ordering::SeqCst);
    let path = req.uri().path().to_owned();
    let csrf = req.headers().get("X-AQ-Request").cloned();
    let raw = to_bytes(req.into_body(), 1_048_576).await.unwrap();
    if path.starts_with("/v1/agent/comments") {
        assert_eq!(csrf.unwrap(), "1");
        return match comment_route(&s, &path, &raw) {
            Ok(value) => (
                [
                    ("X-AQ-Agent-API-Version", "1"),
                    ("Cache-Control", "no-store"),
                ],
                axum::Json(value),
            )
                .into_response(),
            Err(response) => response,
        };
    }
    match route(&s, &path, &raw) {
        Ok(value) => fixture_response(&s, value),
        Err(response) => response,
    }
}

fn fixture_response(s: &Peer, mut value: Value) -> Response {
    if s.mode == 4 {
        return (
            StatusCode::ACCEPTED,
            [("X-AQ-Agent-API-Version", "1")],
            axum::Json(value),
        )
            .into_response();
    }
    if s.mode == 5 {
        value["receipt"]["workspace_id"] = json!(Uuid::new_v4());
    }
    if (10..=14).contains(&s.mode) {
        let name = ["confirmed", "pending", "conflict", "rejected", "unknown"][s.mode - 10];
        let mut result = s.base["operation_statuses"][name].clone();
        result["payload_digest"] = value["payload_digest"].clone();
        result["operation_id"] = value["operation_id"].clone();
        let code = [200, 202, 409, 422, 202][s.mode - 10];
        return (
            StatusCode::from_u16(code).unwrap(),
            [("X-AQ-Agent-API-Version", "1")],
            axum::Json(result),
        )
            .into_response();
    }
    if s.mode == 2 {
        value["binding"]["generation"] = json!(999);
    }
    if s.mode == 3 {
        return (
            StatusCode::OK,
            [("X-AQ-Agent-API-Version", "2")],
            axum::Json(value),
        )
            .into_response();
    }
    (
        [
            ("X-AQ-Agent-API-Version", "1"),
            ("Cache-Control", "no-store"),
        ],
        axum::Json(value),
    )
        .into_response()
}
#[allow(clippy::result_large_err)]
fn route(s: &Peer, path: &str, raw: &[u8]) -> Result<Value, Response> {
    Ok(match path {
        "/v1/agent/context" => s.base["context"].clone(),
        "/v1/agent/graph/read" => graph_read(s, raw),
        "/v1/agent/graph/commands" => graph_command(s, raw)?,
        path if path.starts_with("/v1/agent/operations/") => operation(s),
        "/v1/agent/messages/inbox" => inbox(s, raw),
        "/v1/agent/messages/body" => body(s, raw),
        "/v1/agent/messages/send" => send(s, raw),
        _ => panic!("unexpected finite route"),
    })
}
fn graph_read(s: &Peer, raw: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(raw).unwrap();
    assert_eq!(request["binding"], s.base["context"]["binding"]);
    let mut value = s.base["read_responses"][request["query"]["kind"].as_str().unwrap()].clone();
    value["request_id"] = request["request_id"].clone();
    value
}

#[allow(clippy::result_large_err)]
fn graph_command(s: &Peer, raw: &[u8]) -> Result<Value, Response> {
    let request: Value = serde_json::from_slice(raw).unwrap();
    let mut posts = s.posts.lock().unwrap();
    if let Some(first) = posts.first() {
        assert_eq!(first.as_slice(), raw);
    }
    posts.push(raw.to_vec());
    if s.mode == 1 && posts.len() == 1 {
        return Err((
            StatusCode::BAD_GATEWAY,
            [("X-AQ-Agent-API-Version", "1")],
            "private-secret-marker invalid intermediary response",
        )
            .into_response());
    }
    let mut value = s.base["operation_statuses"]["confirmed"].clone();
    value["operation_id"] = request["operation_id"].clone();
    value["payload_digest"] = json!(digest(&request).unwrap());
    Ok(value)
}

fn operation(s: &Peer) -> Value {
    let request = s
        .posts
        .lock()
        .unwrap()
        .last()
        .map(|b| serde_json::from_slice::<Value>(b).unwrap())
        .unwrap_or_else(|| s.base["command_requests"]["correct_record"].clone());
    let mut value = s.base["operation_statuses"]["confirmed"].clone();
    value["operation_id"] = request["operation_id"].clone();
    value["payload_digest"] = json!(digest(&request).unwrap());
    value
}

fn inbox(s: &Peer, raw: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(raw).unwrap();
    let mut value = messaging("mailbox-page");
    match s.mode {
        20 | 22 => value["messages"][0]["sender"] = supervisor_sender(),
        21 => value["messages"][0]["sender"] = json!({"kind":"owner","user_id":Uuid::from_u128(7)}),
        _ => {}
    }
    if s.mode == 22 && request["limit"] != 1 {
        value = messaging("omitted-body-explicit-chunk-request");
    }
    value["recipient"] = request["recipient"].clone();
    value["after"] = request["after"].clone();
    value
}

fn body(s: &Peer, raw: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(raw).unwrap();
    let mut value = messaging("bounded-body-response");
    value["message_id"] = request["message_id"].clone();
    if request["offset"] != "0" {
        value["offset"] = request["offset"].clone();
        value["next_offset"] = json!("6");
        value["body"] = json!("!");
        value["complete"] = json!(true);
        if s.mode == 23 {
            value["payload_digest"] = json!("b".repeat(64));
        }
    }
    value
}

fn send(s: &Peer, raw: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(raw).unwrap();
    s.posts.lock().unwrap().push(raw.to_vec());
    let mut value = messaging("accepted-is-queued");
    for key in ["operation_id", "message_id", "thread_id", "recipient"] {
        value[key] = request[key].clone();
    }
    value
}
