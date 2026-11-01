//! Finite real loopback peers and held synthetic credential reads; no provider.
use super::*;
use axum::{Router, routing::post};
use std::sync::{Condvar, Mutex, atomic::AtomicUsize};

struct Credential {
    calls: AtomicUsize,
    completed: AtomicUsize,
    block_at: usize,
    entered: tokio::sync::Notify,
    returned: tokio::sync::Notify,
    released: Mutex<bool>,
    gate: Condvar,
    key: Mutex<Zeroizing<Vec<u8>>>,
}
impl Credential {
    fn new(block_at: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            block_at,
            entered: tokio::sync::Notify::new(),
            returned: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            gate: Condvar::new(),
            key: Mutex::new(Zeroizing::new(b"fixture-upstream-key".to_vec())),
        })
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.gate.notify_all();
    }
}
impl RetainedUpstreamCredential for Credential {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.block_at {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.gate.wait(released).unwrap();
            }
        }
        let result = self.key.lock().unwrap().clone();
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.returned.notify_one();
        Ok(result)
    }
}
// A failed assertion cannot strand a blocking reader and hang runtime shutdown.
struct Release(Arc<Credential>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Peer {
    origin: String,
    calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn peer() -> Peer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { ([("content-type", "text/event-stream")], "data: fixture\n\n") }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Peer {
        origin,
        calls,
        task,
    }
}
fn listener() -> (StdTcpListener, String) {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    (listener, origin)
}
fn policy(ttl: Duration) -> ResponsesPolicy {
    let model:ModelSettings=serde_json::from_value(serde_json::json!({"provider":"openai","model":"offline-model","reasoning":"medium","max_output_tokens":100,"spending_budget":null})).unwrap();
    ResponsesPolicy::new(
        Digest::of(b"independent opaque fixture owner"),
        &model,
        boot_ns().unwrap() + ttl.as_nanos() as u64,
    )
    .unwrap()
}
async fn entered(credential: &Credential) {
    tokio::time::timeout(Duration::from_secs(1), credential.entered.notified())
        .await
        .unwrap();
}
async fn completed(credential: &Credential, count: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while credential.completed.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
async fn send(
    owner: &ScopedResponsesTransport,
) -> JoinHandle<Result<reqwest::Response, reqwest::Error>> {
    let endpoint = format!("{}/v1/responses", owner.origin());
    let cap = owner.local_capability();
    tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(endpoint)
            .bearer_auth(std::str::from_utf8(&cap).unwrap())
            .header("content-type", "application/json")
            .body(policy::tests::body())
            .send()
            .await
    })
}

#[tokio::test]
async fn consumed_listener_requires_exact_listening_numeric_loopback_before_credential_read() {
    let credential = Credential::new(0);
    for wrong in [
        "http://localhost:1",
        "http://127.0.0.1:1/path",
        "http://127.0.0.1:1/",
        "http://127.0.0.1:1",
    ] {
        let (listener, _) = listener();
        assert!(
            ScopedResponsesTransport::start_on_listener(
                listener,
                wrong,
                policy(Duration::from_secs(5)),
                UpstreamEndpoint::openai(),
                credential.clone()
            )
            .await
            .is_err()
        );
    }
    let wildcard = StdTcpListener::bind("0.0.0.0:0").unwrap();
    let origin = format!("http://127.0.0.1:{}", wildcard.local_addr().unwrap().port());
    assert!(
        ScopedResponsesTransport::start_on_listener(
            wildcard,
            &origin,
            policy(Duration::from_secs(5)),
            UpstreamEndpoint::openai(),
            credential.clone()
        )
        .await
        .is_err()
    );
    // A connected TCP descriptor cast as TcpListener is still not listening.
    let accepted = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let connection = std::net::TcpStream::connect(accepted.local_addr().unwrap()).unwrap();
    use std::os::fd::{FromRawFd, IntoRawFd};
    let connected = unsafe { StdTcpListener::from_raw_fd(connection.into_raw_fd()) };
    let origin = format!("http://{}", connected.local_addr().unwrap());
    assert!(
        ScopedResponsesTransport::start_on_listener(
            connected,
            &origin,
            policy(Duration::from_secs(5)),
            UpstreamEndpoint::openai(),
            credential.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(credential.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn held_initial_credential_read_is_deadline_bounded_and_late_result_never_serves() {
    for cancel in [true, false] {
        let peer = peer().await;
        let credential = Credential::new(1);
        let _release = Release(credential.clone());
        let (listener, origin) = listener();
        let address = origin.strip_prefix("http://").unwrap().to_owned();
        let current = credential.clone();
        let endpoint = UpstreamEndpoint::development_loopback(&peer.origin).unwrap();
        let task = tokio::spawn(async move {
            ScopedResponsesTransport::start_on_listener(
                listener,
                &origin,
                policy(if cancel {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(150)
                }),
                endpoint,
                current,
            )
            .await
        });
        entered(&credential).await;
        if cancel {
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        } else {
            assert!(
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
        assert!(TcpStream::connect(&address).await.is_err());
        credential.release();
        tokio::time::timeout(Duration::from_secs(1), credential.returned.notified())
            .await
            .unwrap();
        assert_eq!(peer.calls.load(Ordering::SeqCst), 0);
        assert!(TcpStream::connect(&address).await.is_err());
    }
}

#[tokio::test]
async fn held_request_credential_cannot_forward_after_acknowledged_stop_or_expiry() {
    for cancel in [true, false] {
        let peer = peer().await;
        let credential = Credential::new(2);
        let _release = Release(credential.clone());
        let (listener, origin) = listener();
        let owner = ScopedResponsesTransport::start_on_listener(
            listener,
            &origin,
            policy(if cancel {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(150)
            }),
            UpstreamEndpoint::development_loopback(&peer.origin).unwrap(),
            credential.clone(),
        )
        .await
        .unwrap();
        let request = send(&owner).await;
        entered(&credential).await;
        if cancel {
            tokio::time::timeout(Duration::from_secs(1), owner.stop())
                .await
                .unwrap()
                .unwrap();
        } else {
            tokio::time::sleep(Duration::from_millis(170)).await;
            // Joining may report original expiry; either way serving is gone.
            let _ = tokio::time::timeout(Duration::from_secs(1), owner.stop())
                .await
                .unwrap();
        }
        credential.release();
        completed(&credential, 2).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(peer.calls.load(Ordering::SeqCst), 0);
        assert_eq!(credential.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn every_forward_rechecks_credential_and_metadata_pins_remain_original() {
    let peer = peer().await;
    let credential = Credential::new(0);
    let (listener, origin) = listener();
    let scope = policy(Duration::from_secs(5));
    let owner_scope = scope.owner_scope().clone();
    let scope_digest = scope.scope_digest().clone();
    let deadline = scope.original_deadline_boot_ns();
    let owner = ScopedResponsesTransport::start_on_listener(
        listener,
        &origin,
        scope,
        UpstreamEndpoint::development_loopback(&peer.origin).unwrap(),
        credential.clone(),
    )
    .await
    .unwrap();
    assert_eq!(owner.origin(), origin);
    assert_eq!(owner.owner_scope(), &owner_scope);
    assert_eq!(owner.scope_digest(), &scope_digest);
    assert_eq!(owner.original_deadline_boot_ns(), deadline);
    assert_eq!(
        owner.local_capability_digest(),
        Digest::of(&owner.local_capability())
    );
    assert_eq!(
        send(&owner)
            .await
            .await
            .unwrap()
            .unwrap()
            .text()
            .await
            .unwrap(),
        "data: fixture\n\n"
    );
    assert_eq!(credential.calls.load(Ordering::SeqCst), 2);
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    *credential.key.lock().unwrap() = Zeroizing::new(b"changed-source-key".to_vec());
    assert!(send(&owner).await.await.unwrap().is_err());
    assert_eq!(credential.calls.load(Ordering::SeqCst), 3);
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    owner.stop().await.unwrap();
}
