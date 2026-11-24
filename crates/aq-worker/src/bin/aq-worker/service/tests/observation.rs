use super::*;

#[derive(Default)]
pub(super) struct Requests(Arc<std::sync::Mutex<Vec<String>>>);
impl Requests {
    pub(super) fn sent(&self, bearer: &str) -> usize {
        let wanted = format!("Bearer {bearer}");
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|b| **b == wanted)
            .count()
    }
}
pub(super) fn observation_peer() -> (tokio::runtime::Runtime, Requests, String) {
    use axum::{http::HeaderMap, http::StatusCode, routing::any};
    let seen = Requests::default();
    let log = seen.0.clone();
    let app = axum::Router::new().fallback(any(move |h: HeaderMap| {
        let log = log.clone();
        async move {
            let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
            log.lock()
                .unwrap()
                .push(bearer.unwrap_or_default().to_owned());
            StatusCode::SERVICE_UNAVAILABLE
        }
    }));
    // The observation peer is blocking, so it is built outside any async context.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
    (runtime, seen, origin)
}
