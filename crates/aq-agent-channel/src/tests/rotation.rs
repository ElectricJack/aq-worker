use super::*;

/// 0006 §5.2/§5.4: the node rotates before expiry and the caller never sees
/// it; a refused rotation closes the channel and removes the socket.
#[tokio::test]
async fn rotation_is_invisible_and_a_refused_rotation_closes() {
    let (origin, seen) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let issuer = scripted(600, u64::MAX);
    let channel = AgentChannel::open(config(root.path(), &origin, now() + 60_000), issuer.clone())
        .await
        .unwrap();
    let socket = channel.socket_path();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while channel.credential_version() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "rotation did not happen"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, _) = call(&socket, "POST", "/api/v1/commands/whoami", "").await;
    assert_eq!(status, 200);
    let held = seen.lock().unwrap().last().unwrap().auth.clone();
    assert!(
        (2..10).any(|n| held == format!("Bearer {}", token(n))),
        "the caller's call carried a rotated credential"
    );
    // From now on rotation is refused (for example revoked): the channel closes.
    issuer.refuse.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(10), channel.closed())
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
    assert!(issuer.calls.load(Ordering::SeqCst) >= 3);
}

/// 0006 §5.4: revocation closes the channel. The server's `session_revoked`
/// reaches the caller, then the socket directory is gone.
#[tokio::test]
async fn server_revocation_closes_the_channel() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 60_000),
        scripted(60_000, u64::MAX),
    )
    .await
    .unwrap();
    let socket = channel.socket_path();
    let (status, body) = call(&socket, "POST", "/api/v1/commands/revoked", "").await;
    assert_eq!(status, 401);
    assert!(body.contains("session_revoked"));
    tokio::time::timeout(Duration::from_secs(5), channel.closed())
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while channel.directory().exists() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
    // A node close is idempotent after revocation.
    channel.close();
    channel.stop().await.unwrap();
}

/// 0006 §5.4 Launch: a channel that cannot get its credential never creates
/// a socket, and the error is `agent_channel_unavailable`. The grant end
/// closes an open channel.
#[tokio::test]
async fn open_fails_closed_and_the_grant_end_closes() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let error = AgentChannel::open(
        config(root.path(), &origin, now() + 60_000),
        scripted(60_000, 1),
    )
    .await
    .err()
    .unwrap();
    assert!(format!("{error:#}").starts_with(UNAVAILABLE), "{error:#}");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    for bad in [
        "http://example.com",
        "http://localhost:8080",
        "http://127.0.0.1:1/x",
    ] {
        assert!(
            AgentChannel::open(
                config(root.path(), bad, now() + 60_000),
                scripted(60_000, u64::MAX)
            )
            .await
            .is_err(),
            "{bad}"
        );
    }

    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 300),
        scripted(60_000, u64::MAX),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), channel.closed())
        .await
        .unwrap();
    channel.stop().await.unwrap();

    // Restart: a dead node's directory (no lock holder) is swept; a live
    // channel's is not; a stale socket in a reused directory is rebound.
    let stale = root.path().join(Uuid::from_u128(7).to_string());
    std::fs::create_dir(&stale).unwrap();
    drop(std::os::unix::net::UnixListener::bind(stale.join(SOCKET_NAME)).unwrap());
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 60_000),
        scripted(60_000, u64::MAX),
    )
    .await
    .unwrap();
    let (status, _) = call(
        &channel.socket_path(),
        "POST",
        "/api/v1/commands/whoami",
        "",
    )
    .await;
    assert_eq!(status, 200);
    std::fs::create_dir(root.path().join(Uuid::from_u128(8).to_string())).unwrap();
    std::fs::create_dir(root.path().join("not-an-attempt")).unwrap();
    assert_eq!(sweep(root.path()).unwrap(), 1);
    assert!(channel.socket_path().exists() && root.path().join("not-an-attempt").exists());
    channel.stop().await.unwrap();
}

static LIVE_RACE: AtomicU64 = AtomicU64::new(0);

/// Finding 5: the credential is taken just before the upstream send, and a
/// call whose credential a rotation replaced in flight (the server keeps one
/// verifier per attempt and answers `unauthorized`, `not_applied`) is sent
/// once more with the new credential. A 401 for the held version is not.
#[tokio::test]
async fn a_call_that_races_a_rotation_is_resent_with_the_new_credential() {
    let (origin, seen) = stub_with(&LIVE_RACE).await;
    let root = tempfile::tempdir().unwrap();
    let issuer = Arc::new(Scripted {
        ttl_ms: 3_000,
        refuse_from: u64::MAX,
        refuse: Default::default(),
        calls: AtomicU64::new(0),
        live: Some(&LIVE_RACE),
    });
    let channel = AgentChannel::open(config(root.path(), &origin, now() + 60_000), issuer)
        .await
        .unwrap();
    // Sent with version 1; rotation to 2 commits (at 2 s) while it waits.
    let (status, _) = call(
        &channel.socket_path(),
        "POST",
        "/api/v1/commands/live/2",
        "",
    )
    .await;
    assert_eq!(status, 200);
    let auths: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.auth.clone())
        .collect();
    assert_eq!(
        auths,
        [
            format!("Bearer {}", token(1)),
            format!("Bearer {}", token(2))
        ]
    );
    channel.stop().await.unwrap();
}

/// Finding 6: revocation drops the verifier, so calls answer `unauthorized`.
/// That answer reaches the caller (once: the held version did not change),
/// the node re-issues at once, the issuance answers `session_revoked`, and
/// the channel closes, so the next call fails at connect (`channel_closed`).
#[tokio::test]
async fn an_unauthorized_answer_reissues_and_a_revoked_attempt_closes() {
    let (origin, seen) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let issuer = scripted(600_000, u64::MAX);
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 600_000),
        issuer.clone(),
    )
    .await
    .unwrap();
    issuer.refuse.store(true, Ordering::SeqCst);
    let (status, body) = call(
        &channel.socket_path(),
        "POST",
        "/api/v1/commands/unauthorized",
        "",
    )
    .await;
    assert_eq!(status, 401);
    assert!(body.contains("\"unauthorized\""));
    tokio::time::timeout(Duration::from_secs(5), channel.closed())
        .await
        .unwrap();
    assert_eq!(
        issuer.calls.load(Ordering::SeqCst),
        2,
        "open, then one re-issue"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a 401 for the held version is not resent"
    );
    wait_gone(channel.directory()).await;
    assert!(
        tokio::net::UnixStream::connect(channel.socket_path())
            .await
            .is_err()
    );
}

/// Finding 6, bounded: repeated 401s re-issue with exponential backoff
/// (200 ms, 400 ms, 800 ms, … under test; 5 s doubling to 320 s in
/// production), and a re-issue that succeeds keeps the channel open.
#[tokio::test]
async fn unauthorized_answers_reissue_with_exponential_backoff() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let issuer = scripted(600_000, u64::MAX);
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 600_000),
        issuer.clone(),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_millis(2_500) {
        let (status, _) = call(
            &channel.socket_path(),
            "POST",
            "/api/v1/commands/unauthorized",
            "",
        )
        .await;
        assert_eq!(status, 401);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Open, then re-issues at about 0, 0.4, 1.2 and 2.8 s; a fixed 200 ms
    // spacing would have allowed about 13.
    let calls = issuer.calls.load(Ordering::SeqCst);
    assert!((3..=5).contains(&calls), "{calls} issuances");
    assert!(!channel.is_closed());
    channel.stop().await.unwrap();
}

/// Follow-up 1: a caller that asks for another agent API version gets the
/// command surface's `unauthorized` whatever the credential; that never asks
/// for a re-issue. The same 401 without the header does.
#[tokio::test]
async fn a_caller_chosen_api_version_never_forces_a_reissue() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let issuer = scripted(600_000, u64::MAX);
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 600_000),
        issuer.clone(),
    )
    .await
    .unwrap();
    let socket = channel.socket_path();
    for _ in 0..5 {
        let (status, _) = call(
            &socket,
            "POST",
            "/api/v1/commands/unauthorized",
            "X-AQ-Agent-API-Version: 2\r\n",
        )
        .await;
        assert_eq!(status, 401);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(issuer.calls.load(Ordering::SeqCst), 1);
    let (status, _) = call(&socket, "POST", "/api/v1/commands/unauthorized", "").await;
    assert_eq!(status, 401);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while issuer.calls.load(Ordering::SeqCst) < 2 {
        assert!(std::time::Instant::now() < deadline, "no re-issue");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    channel.stop().await.unwrap();
}

/// Follow-up 3: the 401 signal keeps the newest version; a late 401 for an
/// older credential never hides one for the held credential.
#[test]
fn a_late_401_for_an_older_version_keeps_the_newer_signal() {
    let signal = crate::channel::StaleSignal::default();
    signal.raise(2);
    signal.raise(1);
    assert!(signal.raised_for(2));
    assert!(!signal.raised_for(1));
    signal.raise(3);
    assert!(signal.raised_for(3));
}
