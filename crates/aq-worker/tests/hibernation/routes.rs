//! Local stand-in for aq-server's `/v1/worker/lifecycle/*` routes: one
//! `ConformancePeer` per bearer, receipts deduplicated by message id and
//! body digest, and the pending directives for each attempt.
use super::*;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use std::sync::{Arc, Mutex};

pub struct Attempt {
    pub binding: Binding,
    pub peer: ConformancePeer,
    pub directives: Directives,
    pub posts: usize,
}
pub type Shared = Arc<Mutex<BTreeMap<String, Attempt>>>;

#[derive(serde::Deserialize)]
struct AttemptQuery {
    attempt_id: Uuid,
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?;
    Some(value.strip_prefix("Bearer ")?.to_owned())
}

async fn directives(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<AttemptQuery>,
) -> Result<Json<Directives>, StatusCode> {
    let all = shared.lock().unwrap();
    let attempt = bearer(&headers)
        .and_then(|t| all.get(&t))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if attempt.binding.attempt_id != q.attempt_id {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(attempt.directives.clone()))
}

async fn packets(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Receipt>, StatusCode> {
    let mut all = shared.lock().unwrap();
    let attempt = bearer(&headers)
        .and_then(|t| all.get_mut(&t))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let packet = decode(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    attempt.posts += 1;
    match attempt.peer.receive(&packet) {
        Ok(receipt) => Ok(Json(receipt)),
        Err(ContractError::Conflict) => Err(StatusCode::CONFLICT),
        Err(ContractError::Stale) => Err(StatusCode::FORBIDDEN),
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

/// Serve on a background thread; returns the base URL.
pub fn serve(shared: Shared) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/v1/worker/lifecycle/directives", get(directives))
                .route("/v1/worker/lifecycle/packets", post(packets))
                .with_state(shared);
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    base
}
