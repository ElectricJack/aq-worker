use aq_worker::native::runner::sandbox::{private::RunnerFiles, proxy::AqApiProxy};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn runner_private_layout_refuses_unsafe_secret_files() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret("provider.key", b"fixture-provider-key")
        .unwrap();
    let key = files.root().join("provider.key");
    assert_eq!(
        std::fs::metadata(files.root()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&key).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    assert_eq!(
        &*RunnerFiles::read_secret(&key).unwrap(),
        b"fixture-provider-key"
    );
    assert!(files.write_secret("../escape", b"bad").is_err());
    assert!(files.write_secret("provider.key", b"overwrite").is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(RunnerFiles::read_secret(&key).is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&key, files.root().join("link")).unwrap();
    assert!(RunnerFiles::read_secret(&files.root().join("link")).is_err());
    std::fs::hard_link(&key, files.root().join("hardlink")).unwrap();
    assert!(RunnerFiles::read_secret(&key).is_err());
}

#[tokio::test]
async fn aq_proxy_injects_access_header_without_exposing_token() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"runner-only-id","client_secret":"runner-only-secret"}"#,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\nrunner-only-secret",
            "HTTP/1.1 200 OK\r\nCf-Access-Client-Secret: runner-only-secret\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 302 Found\r\nLocation: https://example.invalid/runner-only-secret\r\nContent-Length: 0\r\n\r\n",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap().to_lowercase();
            assert!(head.starts_with("get /v1/agent/context?q=1 http/1.1"));
            assert!(head.contains("authorization: bearer agent-credential\r\n"));
            assert!(head.contains("accept-encoding: identity\r\n"));
            assert!(head.contains("cf-access-client-id: runner-only-id\r\n"));
            assert!(head.contains("cf-access-client-secret: runner-only-secret\r\n"));
            assert!(!head.contains("spoof") && !head.contains("hop-secret"));
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let proxy = AqApiProxy::development_loopback(&upstream, &files.root().join("access.json"))
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for index in 0..4 {
        let response = client
            .get(format!("{}/v1/agent/context?q=1", proxy.origin()))
            .bearer_auth("agent-credential")
            .header("cf-access-client-id", "spoof-id")
            .header("cf-access-client-secret", "spoof-secret")
            .header("Connection", "x-hop")
            .header("x-hop", "hop-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if index == 0 { 200 } else { 502 }
        );
        assert!(response.headers().get("cf-access-client-secret").is_none());
        let body = response.text().await.unwrap();
        assert!(!body.contains("runner-only") && !body.contains("spoof"));
        if index == 0 {
            assert_eq!(body, "{}");
        }
    }
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn runner_responses_proxy_keeps_provider_key_at_upstream() {
    use aq_responses_transport::ResponsesPolicy;
    use aq_session_protocol::{Digest, Text};
    use aq_worker::native::runner::sandbox::responses::RunnerResponsesProxy;
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret("provider.key", b"provider-only-secret")
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap().to_lowercase();
        assert!(head.contains("authorization: bearer provider-only-secret\r\n"));
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["max_output_tokens"],
            100
        );
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 12\r\n\r\ndata: done\n\n").await.unwrap();
    });
    let model = aq_session_protocol::ModelSettings {
        provider: Text::new("openai").unwrap(),
        model: Text::new("fixture-model").unwrap(),
        reasoning: Text::new("medium").unwrap(),
        max_output_tokens: 100,
        spending_budget: None,
    };
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) },
        0
    );
    let policy = ResponsesPolicy::new(
        Digest::of(b"fixture-attempt"),
        &model,
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 30_000_000_000,
    )
    .unwrap();
    let proxy = RunnerResponsesProxy::development_loopback(
        policy,
        &files.root().join("provider.key"),
        &upstream,
    )
    .await
    .unwrap();
    let local = proxy.local_capability();
    assert_ne!(&*local, b"provider-only-secret");
    let response = reqwest::Client::new().post(format!("{}/v1/responses", proxy.origin()))
        .bearer_auth(std::str::from_utf8(&local).unwrap()).header("content-type", "application/json")
        .body(r#"{"model":"fixture-model","stream":true,"store":false,"input":[],"reasoning":{"effort":"medium","summary":"auto"},"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"text":{"verbosity":"low"},"include":[]}"#)
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "data: done\n\n");
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}

// A fresh dedicated executable performs transitions before starting any threads.
#[test]
fn agent_user_cannot_read_permit_or_proxy_key() {
    let wrapper = std::env::var("AQ_B6B_ROOT_WRAPPER").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/test-b6b-root.sh"
        )
        .into()
    });
    let output = std::process::Command::new(wrapper)
        .arg(env!("CARGO_BIN_EXE_aq-b6b-user-probe"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn aq_proxy_refuses_non_machine_origins_and_unsafe_access_files() {
    let dir = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&dir.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"id","client_secret":"secret"}"#,
        )
        .unwrap();
    for origin in [
        "http://machine.example",
        "https://127.0.0.1",
        "https://[::1]",
        "https://[2001:db8::1]",
        "https://user:pass@machine.example",
        "https://machine.example/route",
        "https://machine.example/?x=1",
    ] {
        assert!(
            AqApiProxy::start(origin, &files.root().join("access.json"))
                .await
                .is_err()
        );
    }
    for origin in [
        "http://0.0.0.0:1234",
        "http://localhost:1234",
        "http://127.0.0.1:1234/route",
    ] {
        assert!(
            AqApiProxy::development_loopback(origin, &files.root().join("access.json"))
                .await
                .is_err()
        );
    }
    std::fs::set_permissions(
        files.root().join("access.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(
        AqApiProxy::development_loopback(
            "http://127.0.0.1:1234",
            &files.root().join("access.json")
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn aq_proxy_shutdown_cancels_concurrent_incomplete_requests() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"fixture-id","client_secret":"fixture-secret"}"#,
        )
        .unwrap();
    let proxy =
        AqApiProxy::development_loopback("http://127.0.0.1:9", &files.root().join("access.json"))
            .await
            .unwrap();
    let address = proxy.origin().strip_prefix("http://").unwrap().to_owned();
    let mut sockets = Vec::new();
    for _ in 0..8 {
        let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
        socket.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        sockets.push(socket);
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), proxy.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(&address).await.is_err());
    for mut socket in sockets {
        let mut byte = [0];
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), socket.read(&mut byte))
                .await
                .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
    }
}

#[test]
fn runner_private_files_refuse_oversized_and_malformed_secrets() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    assert!(
        files
            .write_secret("provider.key", &vec![b'x'; 4 * 1024 * 1024 + 1])
            .is_err()
    );
    files.write_secret("provider.key", b"key").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(files.root().join("provider.key"))
        .unwrap();
    file.set_len(4 * 1024 * 1024 + 1).unwrap();
    assert!(RunnerFiles::read_secret(&files.root().join("provider.key")).is_err());
}

#[tokio::test]
async fn aq_proxy_refuses_compressed_access_echo() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"runner-only-id","client_secret":"runner-only-secret"}"#,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        // gzip({"echo":"runner-only-secret"}), containing no literal token bytes.
        let compressed: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 171, 86, 74, 77, 206, 200, 87, 178, 82, 42, 42, 205,
            203, 75, 45, 210, 205, 207, 203, 169, 212, 45, 78, 77, 46, 74, 45, 81, 170, 5, 0, 124,
            32, 219, 126, 29, 0, 0, 0,
        ];
        socket
            .write_all(
                format!(
                    "HTTP/1.1 500 Error\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                    compressed.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(compressed).await.unwrap();
    });
    let proxy = AqApiProxy::development_loopback(&upstream, &files.root().join("access.json"))
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!("{}/echo", proxy.origin()))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(response.bytes().await.unwrap().is_empty());
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}
