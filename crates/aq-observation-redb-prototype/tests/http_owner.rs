use aq_observation_admission::{self as a, Operation, Request};
use aq_observation_redb_prototype::{AuthenticatedOwnerPeer, HttpOwnerConfig, HttpOwnerPeer};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;
fn config(root: &std::path::Path, origin: String) -> HttpOwnerConfig {
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = root.join("node.token");
    std::fs::write(&token, "aqn_synthetic_owner_token_0123456789").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    HttpOwnerConfig {
        version: 1,
        origin,
        node_id: Uuid::from_u128(5),
        boot_id: Uuid::from_u128(6),
        credential_file: token,
        allow_loopback_http: true,
    }
}
fn request() -> Request {
    Request {
        version: 1,
        operation_id: Uuid::from_u128(401),
        node_id: Uuid::from_u128(5),
        operation: Operation::Prepare {
            boot_id: Uuid::from_u128(6),
            store_id: Uuid::from_u128(402),
        },
    }
}
fn reply() -> Vec<u8> {
    let r = request();
    serde_json::to_vec(
        &a::apply(
            None,
            None,
            &r,
            &a::Access {
                node_id: r.node_id,
                boot_id: Uuid::from_u128(6),
                accepted_through: 0,
                max_pending_distance: a::MAX_PENDING_DISTANCE,
                new_boot_previously_owned: false,
            },
            Some(Uuid::from_u128(400)),
        )
        .unwrap()
        .reply,
    )
    .unwrap()
}
fn serve(response: Vec<u8>) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let h = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut captured = vec![];
        while std::time::Instant::now() < deadline {
            match l.accept() {
                Ok((mut s, _)) => {
                    c.fetch_add(1, Ordering::SeqCst);
                    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut b = [0; 2048];
                    let mut bytes = vec![];
                    loop {
                        let n = s.read(&mut b).unwrap();
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&b[..n]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&bytes[..end]);
                            let len = header
                                .lines()
                                .find_map(|s| {
                                    s.split_once(':')
                                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    captured = bytes;
                    s.write_all(&response).ok();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => panic!("{e}"),
            }
        }
        captured
    });
    (origin, count, h)
}
fn response(code: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut b = format!(
        "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    b.extend_from_slice(body);
    b
}
#[test]
fn exact_http_route_auth_and_bounded_valid_reply() {
    let t = tempfile::tempdir().unwrap();
    let (origin, count, h) = serve(response(
        "200 OK",
        "Content-Type: application/json\r\nCache-Control: no-store\r\n",
        &reply(),
    ));
    let cfg = config(t.path(), origin);
    let mut peer = HttpOwnerPeer::from_config(&cfg).unwrap();
    let r = peer.mutate(&request()).unwrap();
    assert!(r.may_create(&request()).unwrap());
    let raw = h.join().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let raw = String::from_utf8(raw).unwrap();
    assert!(raw.starts_with("POST /v1/worker/observations/transport HTTP/1.1\r\n"));
    assert!(
        raw.to_ascii_lowercase()
            .contains("authorization: bearer aqn_synthetic_owner_token_0123456789")
    );
    assert!(raw.to_ascii_lowercase().contains("cache-control: no-store"));
}
#[test]
fn dropped_reply_never_retries_and_errors_do_not_expose_credentials() {
    let t = tempfile::tempdir().unwrap();
    let (origin, count, h) = serve(vec![]);
    let mut peer = HttpOwnerPeer::from_config(&config(t.path(), origin)).unwrap();
    let e = peer.mutate(&request()).unwrap_err().to_string();
    assert!(!e.contains("aqn_") && !e.contains("0123456789"));
    h.join().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[test]
fn redirect_missing_cache_malformed_oversized_and_encoded_replies_refused() {
    for raw in [
        response("302 Found", "Location: http://127.0.0.1:1/secret\r\n", &[]),
        response("200 OK", "Content-Type: application/json\r\n", &reply()),
        response(
            "200 OK",
            "Content-Type: application/json\r\nCache-Control: no-store\r\n",
            b"{broken",
        ),
        response(
            "200 OK",
            "Content-Type: application/json\r\nCache-Control: no-store\r\n",
            &vec![b' '; 16385],
        ),
        response(
            "200 OK",
            "Content-Type: application/json\r\nCache-Control: no-store\r\nContent-Encoding: gzip\r\n",
            &reply(),
        ),
    ] {
        let t = tempfile::tempdir().unwrap();
        let (origin, count, h) = serve(raw);
        let mut p = HttpOwnerPeer::from_config(&config(t.path(), origin)).unwrap();
        assert!(p.mutate(&request()).is_err());
        h.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn unsafe_origin_private_input_and_cross_node_requests_refused_without_network() {
    let t = tempfile::tempdir().unwrap();
    let mut cfg = config(t.path(), "http://127.0.0.1:1".into());
    for origin in [
        "http://example.com",
        "http://localhost:1",
        "https://user:secret@example.com",
        "https://example.com/a",
        "https://example.com?secret=x",
        "https://example.com/#x",
        "http://127.0.0.1:0",
        "http://127.0.0.1:1/a/..",
    ] {
        cfg.origin = origin.into();
        assert!(HttpOwnerPeer::from_config(&cfg).is_err());
    }
    cfg.origin = "http://127.0.0.1:1".into();
    cfg.allow_loopback_http = false;
    assert!(HttpOwnerPeer::from_config(&cfg).is_err());
    cfg.allow_loopback_http = true;
    let mut p = HttpOwnerPeer::from_config(&cfg).unwrap();
    let mut r = request();
    r.node_id = Uuid::from_u128(999);
    assert!(
        p.mutate(&r)
            .unwrap_err()
            .to_string()
            .contains("node mismatch")
    );
    assert!(p.status(r.node_id).is_err());
    std::fs::set_permissions(&cfg.credential_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(HttpOwnerPeer::from_config(&cfg).is_err());
    std::fs::set_permissions(&cfg.credential_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token = cfg.credential_file.clone();
    cfg.credential_file = t.path().join("linked");
    std::os::unix::fs::symlink(token, &cfg.credential_file).unwrap();
    assert!(HttpOwnerPeer::from_config(&cfg).is_err());
}
