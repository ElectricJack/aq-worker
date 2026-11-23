/// The three tokens of a ChatGPT login are the secrets the driver refuses to let out.
#[test]
fn codex_login_tokens_are_the_three_login_secrets() {
    use aq_handoff_note::{Secrets, harness::codex_login_tokens};
    let (auth, refresh) = codex_login();
    let tokens = codex_login_tokens(&auth).unwrap();
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[2].as_slice(), refresh.as_bytes());
    let by = Secrets::of_tokens(tokens.iter().map(|t| t.as_slice()).collect(), None);
    for token in &tokens {
        assert!(by.holds(&[b"leak ".as_slice(), token].concat()));
    }
    assert!(!by.holds(b"fixture-refresh"));
    let api = json!({"OPENAI_API_KEY": "fixture-key-0123456789"}).to_string();
    assert!(codex_login_tokens(api.as_bytes()).is_err());
    assert!(codex_login_tokens(b"not json").is_err());
}

/// C5b for the docker-local sandbox: a Codex login that the stopped sandbox holds
/// refreshed (same account, later `last_refresh`) is written back to the server as
/// the next version of that attempt's lease; an unchanged or foreign login, another
/// attempt's sandbox, and an attempt without a lease file write nothing.
#[tokio::test]
async fn refreshed_codex_login_is_written_back_from_the_stopped_sandbox() {
    use aq_agent_channel::login::{LEASE_FILE, LoginLease, WriteBack};
    use aq_worker::native::runner::write_back_login;
    let login = |account: &str, refreshed: &str| {
        json!({"auth_mode": "chatgpt", "last_refresh": refreshed,
            "tokens": {"id_token": "fixture-id", "access_token": "fixture-access",
                "refresh_token": "fixture-refresh", "account_id": account}})
        .to_string()
        .into_bytes()
    };
    let leased = login("fixture-account", "2026-11-12T00:00:00Z");
    let refreshed = login("fixture-account", "2026-11-18T00:00:00Z");

    let (link, uploads) = login_writeback_receiver().await;
    let driver = tempfile::tempdir().unwrap();
    let lease = LoginLease::of(&leased, json!({}), json!({}), Uuid::from_u128(9), 3).unwrap();
    // One sandbox per case: what the stopped sandbox's auth.json holds.
    let cases = [
        (3, refreshed.clone(), Some(WriteBack::Accepted(4))),
        (4, leased.clone(), None),
        (5, login("another-account", "2026-11-18T00:00:00Z"), None),
        (6, b"{not a login".to_vec(), None),
    ];
    for (attempt, held, expected) in cases {
        let docker = ScriptedDocker::new().runner(Runner::StaysRunning);
        docker.on_start_write("/aq/home/codex/auth.json", held);
        let mut h = harness_with(docker, |_| {}).await;
        let request = self::request(1, 2, attempt);
        let sandbox = bounded(h.backend.create(&request)).await.unwrap();
        let material = PrivateMaterial::new(b"fixture".to_vec());
        bounded(h.backend.deliver_private_material(&sandbox, material))
            .await
            .unwrap();
        bounded(h.backend.start_runner(&sandbox)).await.unwrap();
        assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
        let ids = (Uuid::from_u128(1), Uuid::from_u128(attempt));
        // No lease file (a Claude attempt): nothing is read or sent.
        let none = write_back_login(&mut h.backend, ids, driver.path(), &link).await;
        assert_eq!(none.unwrap(), None);
        write_private_fixture(
            &driver.path().join(LEASE_FILE),
            &serde_json::to_vec(&lease).unwrap(),
        );
        let other = (ids.0, Uuid::from_u128(99));
        let foreign = write_back_login(&mut h.backend, other, driver.path(), &link).await;
        assert_eq!(
            foreign.unwrap(),
            None,
            "another attempt's sandbox is not read"
        );
        let written = write_back_login(&mut h.backend, ids, driver.path(), &link).await;
        assert_eq!(written.unwrap(), expected, "attempt {attempt}");
        std::fs::remove_file(driver.path().join(LEASE_FILE)).unwrap();
    }
    assert_eq!(
        *uploads.lock().unwrap(),
        vec![refreshed],
        "only the refreshed login left the node"
    );
}

/// Moved from `native::launch`, to keep the library within its line budget.
#[test]
fn leftover_token_is_wiped_and_a_missing_one_is_fine() {
    use aq_worker::native::launch::wipe_claude_driver_secrets;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, b"x").unwrap();
    std::fs::write(dir.path().join("exit.json"), b"{}").unwrap();
    wipe_claude_driver_secrets(dir.path());
    assert!(!token.exists());
    assert!(
        dir.path().join("exit.json").exists(),
        "non-secret records stay"
    );
    wipe_claude_driver_secrets(dir.path());
}

async fn login_writeback_receiver() -> (
    aq_agent_channel::NodeLink,
    std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) {
    use axum::{Router, body::Bytes, routing::post};
    let uploads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let seen = uploads.clone();
    let app = Router::new().route(
        "/v1/worker/profiled/material/refresh",
        post(move |body: Bytes| async move {
            seen.lock().unwrap().push(body.to_vec());
            r#"{"accepted":true,"version":4,"replayed":false}"#
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let link =
        aq_agent_channel::NodeLink::new(&origin, "node-credential-0123456789", None, None).unwrap();

    (link, uploads)
}
