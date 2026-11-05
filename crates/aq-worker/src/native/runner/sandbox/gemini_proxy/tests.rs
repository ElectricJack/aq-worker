use super::*;
use tokio::net::TcpListener;

const KEY: &str = "AIzaFixtureProviderOnlyKey_0123";

async fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files.write_secret("provider.key", KEY.as_bytes()).unwrap();
    let key = files.root().join("provider.key");
    (root, key)
}

/// Accepts one request, returns its lower-cased head and body.
async fn upstream(reply: &'static [u8]) -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap().to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        socket.write_all(reply).await.unwrap();
        (head, body)
    });
    (origin, peer)
}

const PATH: &str = "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse";

#[tokio::test]
async fn swaps_capability_for_runner_key_on_generation_paths() {
    let (_root, key) = fixture().await;
    let (origin, peer) = upstream(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 12\r\n\r\ndata: done\n\n",
    )
    .await;
    let proxy = GeminiApiProxy::development_loopback(&origin, &key)
        .await
        .unwrap();
    let capability = proxy.local_capability().to_owned();
    assert!(is_capability(&capability));
    assert!(!capability.contains(KEY));
    let response = reqwest::Client::new()
        .post(format!("{}{PATH}", proxy.origin()))
        .header("x-goog-api-key", &capability)
        .header("x-gemini-api-privileged-user-id", "install-id")
        .header("accept-encoding", "gzip, deflate")
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "data: done\n\n");
    let (head, body) = peer.await.unwrap();
    assert!(head.starts_with(&format!("post {} http/1.1", PATH.to_lowercase())));
    assert!(head.contains(&format!("x-goog-api-key: {}\r\n", KEY.to_lowercase())));
    assert!(!head.contains(&capability));
    assert!(head.contains("accept-encoding: identity\r\n"));
    assert!(!head.contains("privileged-user-id"));
    assert_eq!(body, br#"{"contents":[]}"#);
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn refuses_bad_capability_and_paths_before_any_upstream_call() {
    let (_root, key) = fixture().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let proxy = GeminiApiProxy::development_loopback(&origin, &key)
        .await
        .unwrap();
    let good = proxy.local_capability().to_owned();
    let client = reqwest::Client::new();
    for (path, capability) in [
        (PATH, "aq-gemini-wrong"),
        (PATH, KEY),
        (
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse&key=x",
            good.as_str(),
        ),
        ("/v1beta/files", good.as_str()),
        ("/v1beta/models/../files:generateContent", good.as_str()),
        (
            "/v1beta/models/gemini-2.5-flash:embedContent",
            good.as_str(),
        ),
        ("/v1/models/gemini-2.5-flash:generateContent", good.as_str()),
    ] {
        let status = client
            .post(format!("{}{path}", proxy.origin()))
            .header("x-goog-api-key", capability)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 502, "{path} {capability}");
    }
    let status = client
        .get(format!("{}{PATH}", proxy.origin()))
        .header("x-goog-api-key", &good)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 502);
    // No request reached the upstream listener.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn refuses_upstream_echo_of_the_key_and_encoded_bodies() {
    let (_root, key) = fixture().await;
    for reply in [
        b"HTTP/1.1 400 Bad\r\nContent-Length: 31\r\n\r\nAIzaFixtureProviderOnlyKey_0123" as &[u8],
        b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nxx",
        b"HTTP/1.1 302 Found\r\nLocation: https://example.com/\r\nContent-Length: 0\r\n\r\n",
    ] {
        let (origin, peer) = upstream(Box::leak(reply.to_vec().into_boxed_slice())).await;
        let proxy = GeminiApiProxy::development_loopback(&origin, &key)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}{PATH}", proxy.origin()))
            .header("x-goog-api-key", proxy.local_capability())
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
        assert!(!response.text().await.unwrap().contains(KEY));
        peer.await.unwrap();
        proxy.stop().await.unwrap();
    }
}

#[tokio::test]
async fn refuses_non_loopback_fixture_and_malformed_key_files() {
    let (root, key) = fixture().await;
    for origin in [
        "https://127.0.0.1:1",
        "http://localhost:1",
        "http://127.0.0.1",
    ] {
        assert!(
            GeminiApiProxy::development_loopback(origin, &key)
                .await
                .is_err()
        );
    }
    let files = RunnerFiles::create(&root.path().join("other")).unwrap();
    files.write_secret("provider.key", b"has space\n").unwrap();
    assert!(
        GeminiApiProxy::development_loopback(
            "http://127.0.0.1:9",
            &files.root().join("provider.key")
        )
        .await
        .is_err()
    );
}
