//! C6b: install, retry-once, fail-fast, CAS write-back and wipe of a leased
//! subscription login, against a fake lease source / C5b server.
use super::private::RunnerFiles;
use super::subscription_login::*;
use anyhow::Result;
use aq_session_protocol::Digest;
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path, time::Duration};
use tokio::time::Instant;
use uuid::Uuid;
use zeroize::Zeroizing;

type Rewrite = Box<dyn FnMut(&Path)>;
type Replies = std::sync::Arc<std::sync::Mutex<VecDeque<(u16, &'static str)>>>;

const LEASE: Uuid = Uuid::from_u128(0xC6B);

#[derive(Default)]
struct FakeServer {
    newest: VecDeque<IssuedLogin>,
    fetches: usize,
    answers: VecDeque<Result<WriteBack>>,
    uploads: Vec<(Uuid, i64, Vec<u8>)>,
}
impl LoginService for FakeServer {
    async fn fetch_newest(&mut self, kind: LoginKind) -> Result<IssuedLogin> {
        self.fetches += 1;
        let next = if self.newest.len() > 1 {
            self.newest.pop_front().unwrap()
        } else {
            let last = self.newest.front().unwrap();
            issued(kind, last.version, &last.material)
        };
        Ok(next)
    }
    async fn write_back(&mut self, lease: Uuid, base: i64, bytes: &[u8]) -> Result<WriteBack> {
        self.uploads.push((lease, base, bytes.to_vec()));
        self.answers
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("server unavailable")))
    }
}
fn issued(kind: LoginKind, version: i64, material: &[u8]) -> IssuedLogin {
    IssuedLogin {
        kind,
        lease_id: LEASE,
        version,
        material: Zeroizing::new(material.to_vec()),
    }
}
fn server(logins: Vec<IssuedLogin>) -> FakeServer {
    FakeServer {
        newest: logins.into(),
        ..Default::default()
    }
}
fn codex_login(refresh: &str, last_refresh: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
        "tokens":{"id_token":"offline-id","access_token":"offline-access",
            "refresh_token":refresh,"account_id":"offline-account"},
        "last_refresh":last_refresh}))
    .unwrap()
}

/// Records each launch's login and returns scripted exits; `rewrite` is what
/// a Codex harness leaves in `auth.json` (a refresh, a symlink, junk).
struct FakeHarness {
    exits: VecDeque<HarnessExit>,
    seen: Vec<(i64, Option<String>)>,
    rewrite: Option<Rewrite>,
}
impl FakeHarness {
    fn new(exits: &[HarnessExit]) -> Self {
        Self {
            exits: exits.iter().copied().collect(),
            seen: vec![],
            rewrite: None,
        }
    }
}
impl Harness for FakeHarness {
    async fn launch(&mut self, login: &InstalledLogin) -> Result<HarnessExit> {
        self.seen.push((
            login.version(),
            login.claude_env().map(|(name, token)| {
                assert_eq!(name, "CLAUDE_CODE_OAUTH_TOKEN");
                token.to_owned()
            }),
        ));
        if let (Some(rewrite), Some(home)) = (
            self.rewrite.as_mut(),
            login
                .codex_home()
                .or(login.gemini_state())
                .or(login.opencode_data_dir()),
        ) {
            rewrite(home);
        }
        Ok(self.exits.pop_front().unwrap())
    }
}

struct Dirs {
    _root: tempfile::TempDir,
    home: std::path::PathBuf,
    journal: std::path::PathBuf,
    files: RunnerFiles,
}
fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("journal");
    std::fs::create_dir(&journal).unwrap();
    Dirs {
        home: root.path().join("codex-home"),
        files: RunnerFiles::create(&root.path().join("runner")).unwrap(),
        journal,
        _root: root,
    }
}
fn journaled(dirs: &Dirs) -> Value {
    serde_json::from_slice(&std::fs::read(dirs.journal.join("login-writeback.json")).unwrap())
        .unwrap()
}
fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[tokio::test]
async fn expired_subscription_login_fails_fast_with_credential_reason() {
    use HarnessExit::AuthenticationFailed as Auth;
    // The server holds nothing newer than the expired token: fail fast.
    let d = dirs();
    let mut service = server(vec![issued(LoginKind::ClaudeOauthToken, 3, b"sk-expired")]);
    let mut login = SubscriptionLogin::start(&mut service, LoginKind::ClaudeOauthToken, None)
        .await
        .unwrap();
    let mut harness = FakeHarness::new(&[Auth, Auth]);
    let outcome = login.run(&mut service, &mut harness).await.unwrap();
    let AttemptOutcome::Failed(failure) = outcome else {
        panic!("expected a login failure, got {outcome:?}");
    };
    assert_eq!(failure, LoginFailure::CREDENTIAL_INVALID);
    assert_eq!(
        serde_json::to_value(failure).unwrap(),
        json!({"reason":"credential_invalid","retryable":false})
    );
    assert_eq!(harness.seen.len(), 1, "no relaunch without a newer login");
    assert_eq!(service.fetches, 2, "install fetch plus one newest recheck");
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
    assert!(service.uploads.is_empty());

    // A newer version earns exactly one relaunch; a second 401 is final.
    let mut service = server(vec![
        issued(LoginKind::ClaudeOauthToken, 3, b"sk-expired"),
        issued(LoginKind::ClaudeOauthToken, 4, b"sk-rotated"),
    ]);
    let mut login = SubscriptionLogin::start(&mut service, LoginKind::ClaudeOauthToken, None)
        .await
        .unwrap();
    let mut harness = FakeHarness::new(&[Auth, Auth, Auth]);
    let outcome = login.run(&mut service, &mut harness).await.unwrap();
    assert_eq!(
        outcome,
        AttemptOutcome::Failed(LoginFailure::CREDENTIAL_INVALID)
    );
    assert_eq!(
        harness.seen,
        vec![
            (3, Some("sk-expired".into())),
            (4, Some("sk-rotated".into()))
        ]
    );

    // A newer version that works completes the attempt.
    let mut service = server(vec![
        issued(LoginKind::ClaudeOauthToken, 3, b"sk-expired"),
        issued(LoginKind::ClaudeOauthToken, 4, b"sk-rotated"),
    ]);
    let mut login = SubscriptionLogin::start(&mut service, LoginKind::ClaudeOauthToken, None)
        .await
        .unwrap();
    let mut harness = FakeHarness::new(&[Auth, HarnessExit::Completed]);
    assert_eq!(
        login.run(&mut service, &mut harness).await.unwrap(),
        AttemptOutcome::Exited(HarnessExit::Completed)
    );
}

fn refresh_to(bytes: Vec<u8>) -> Option<Rewrite> {
    Some(Box::new(move |home: &Path| {
        std::fs::write(home.join("auth.json"), &bytes).unwrap();
    }))
}
async fn codex_attempt(
    d: &Dirs,
    service: &mut FakeServer,
    rewrite: Option<Rewrite>,
) -> SubscriptionLogin {
    let mut login = SubscriptionLogin::start(service, LoginKind::CodexChatgptAuth, Some(&d.home))
        .await
        .unwrap();
    let auth = d.home.join("auth.json");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&auth).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    let mut harness = FakeHarness::new(&[HarnessExit::Completed]);
    harness.rewrite = rewrite;
    assert_eq!(
        login.run(service, &mut harness).await.unwrap(),
        AttemptOutcome::Exited(HarnessExit::Completed)
    );
    login
}

#[tokio::test]
async fn refreshed_login_written_back_before_teardown() {
    let d = dirs();
    let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
    let refreshed = codex_login("refresh-v8", "2026-11-04T12:00:00Z");
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    service
        .answers
        .push_back(Err(anyhow::anyhow!("transient 503")));
    service.answers.push_back(Ok(WriteBack::Accepted {
        version: 8,
        replayed: false,
    }));
    let login = codex_attempt(&d, &mut service, refresh_to(refreshed.clone())).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(
        service.uploads,
        vec![(LEASE, 7, refreshed.clone()), (LEASE, 7, refreshed.clone())],
        "N+1 based on N, retried past a transient failure"
    );
    assert_eq!(receipt.outcome, ReceiptOutcome::Accepted);
    assert_eq!(receipt.version, Some(8));
    assert_eq!(receipt.base_version, 7);
    assert_eq!(receipt.content_digest, Some(Digest::of(&refreshed)));
    assert!(!receipt.retained);
    assert!(!d.home.exists(), "login home wiped");
    assert!(!d.files.root().join("refreshed-login.json").exists());
    let journal = journaled(&d);
    assert_eq!(journal["outcome"], "accepted");
    assert_eq!(journal["version"], 8);
    let raw = std::fs::read_to_string(d.journal.join("login-writeback.json")).unwrap();
    assert!(!raw.contains("refresh-v8") && !raw.contains("offline-access"));
}

#[tokio::test]
async fn unchanged_or_stale_login_is_not_uploaded() {
    let d = dirs();
    let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    let login = codex_attempt(&d, &mut service, None).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
    // Different bytes but no newer last_refresh: not a refresh.
    let d = dirs();
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    let stale = codex_login("refresh-other", "2026-10-01T00:00:00Z");
    let login = codex_attempt(&d, &mut service, refresh_to(stale)).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
    assert!(service.uploads.is_empty() && !d.home.exists());
}

#[tokio::test]
async fn invalid_refreshed_login_is_not_uploaded() {
    let d = dirs();
    let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    let login = codex_attempt(
        &d,
        &mut service,
        refresh_to(b"{\"auth_mode\":\"chatgpt\"".to_vec()),
    )
    .await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
    assert!(service.uploads.is_empty() && !d.home.exists());
}

#[tokio::test]
async fn reconcile_refuses_symlinked_login_and_still_wipes() {
    let d = dirs();
    let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
    let decoy = d.journal.join("decoy.json");
    std::fs::write(&decoy, codex_login("refresh-decoy", "2026-11-05T00:00:00Z")).unwrap();
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    let target = decoy.clone();
    let swap: Rewrite = Box::new(move |home: &Path| {
        std::fs::remove_file(home.join("auth.json")).unwrap();
        std::os::unix::fs::symlink(&target, home.join("auth.json")).unwrap();
    });
    let login = codex_attempt(&d, &mut service, Some(swap)).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
    assert!(service.uploads.is_empty() && !d.home.exists() && decoy.exists());
}

#[tokio::test]
async fn superseded_write_back_discards_refreshed_login() {
    let d = dirs();
    let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
    let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
    service
        .answers
        .push_back(Ok(WriteBack::Superseded { version: 9 }));
    let refreshed = codex_login("refresh-v8", "2026-11-04T12:00:00Z");
    let login = codex_attempt(&d, &mut service, refresh_to(refreshed)).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::Superseded);
    assert_eq!(receipt.version, Some(9));
    assert_eq!(service.uploads.len(), 1, "a newer version wins; no retry");
    assert!(!receipt.retained && !d.home.exists());
    assert!(!d.files.root().join("refreshed-login.json").exists());
}

#[tokio::test]
async fn failed_write_back_retains_runner_only_copy() {
    for (answer, outcome) in [
        (Some(WriteBack::NotRenewable), ReceiptOutcome::NotRenewable),
        (None, ReceiptOutcome::DeadlineExceeded),
    ] {
        let d = dirs();
        let original = codex_login("refresh-v7", "2026-11-01T00:00:00Z");
        let refreshed = codex_login("refresh-v8", "2026-11-04T12:00:00Z");
        let mut service = server(vec![issued(LoginKind::CodexChatgptAuth, 7, &original)]);
        service.answers.extend(answer.map(Ok));
        let login = codex_attempt(&d, &mut service, refresh_to(refreshed.clone())).await;
        let deadline = Instant::now() + Duration::from_millis(150);
        let receipt = login
            .finish(&mut service, &d.journal, &d.files, deadline)
            .await
            .unwrap();
        assert_eq!(receipt.outcome, outcome);
        assert!(receipt.retained && !d.home.exists());
        let kept = RunnerFiles::read_secret(&d.files.root().join("refreshed-login.json")).unwrap();
        assert_eq!(&kept[..], &refreshed[..]);
        assert_eq!(journaled(&d)["retained"], true);
    }
}

#[test]
fn login_debug_redacts_material() {
    let login = issued(LoginKind::ClaudeOauthToken, 1, b"sk-secret-token");
    assert!(!format!("{login:?}").contains("sk-secret-token"));
}

#[tokio::test]
async fn runner_write_back_speaks_c5b_contract() {
    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode, Uri},
        routing::any,
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::sync::{Arc, Mutex};
    type Seen = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
    let replies: Replies = Arc::new(Mutex::new(
        [
            (200, r#"{"accepted":true,"version":8,"replayed":false}"#),
            (409, r#"{"accepted":false,"version":9,"replayed":false}"#),
            (
                409,
                r#"{"error":"material refresh is not currently authorized"}"#,
            ),
            (503, "{}"),
        ]
        .into(),
    ));
    let seen = Seen::default();
    async fn record(
        State((seen, replies)): State<(Seen, Replies)>,
        uri: Uri,
        headers: HeaderMap,
        bytes: Bytes,
    ) -> (StatusCode, &'static str) {
        seen.lock()
            .unwrap()
            .push((uri.to_string(), headers, bytes.to_vec()));
        let (status, body) = replies.lock().unwrap().pop_front().unwrap();
        (StatusCode::from_u16(status).unwrap(), body)
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(any(record))
        .with_state((seen.clone(), replies));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_dir, http, permit) = super::auth_tests::client(&origin, false);
    let body = codex_login("refresh-v8", "2026-11-04T12:00:00Z");
    assert_eq!(
        http.refresh_login(LEASE, 7, &body).await.unwrap(),
        WriteBack::Accepted {
            version: 8,
            replayed: false
        }
    );
    assert_eq!(
        http.refresh_login(LEASE, 7, &body).await.unwrap(),
        WriteBack::Superseded { version: 9 }
    );
    assert_eq!(
        http.refresh_login(LEASE, 7, &body).await.unwrap(),
        WriteBack::NotRenewable
    );
    assert!(
        http.refresh_login(LEASE, 7, &body).await.is_err(),
        "5xx is transient"
    );
    assert!(!http.authority_lost(), "CAS answers are not authority loss");
    task.abort();
    let seen = seen.lock().unwrap();
    let (path, headers, sent) = &seen[0];
    assert_eq!(path, "/v1/worker/profiled/material/refresh");
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(
        headers["authorization"],
        "Bearer fixture_only_not_a_real_secret_00000000"
    );
    assert_eq!(sent, &body);
    let refresh: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(headers["aq-material-refresh"].as_bytes())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(refresh["schema_version"], 1);
    assert_eq!(refresh["lease_id"], LEASE.to_string());
    assert_eq!(refresh["base_version"], 7);
    assert_eq!(
        refresh["content_digest"],
        serde_json::to_value(Digest::of(&body)).unwrap()
    );
    assert_eq!(
        refresh["identity"]["node_id"],
        serde_json::to_value(permit.binding.node.node_id).unwrap()
    );
    assert_eq!(
        refresh["identity"]["boot_id"],
        serde_json::to_value(permit.binding.node.boot_id).unwrap()
    );
    assert_eq!(
        refresh["binding"],
        serde_json::to_value(http.launch_binding()).unwrap()
    );
    let keys: Vec<_> = refresh.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 7, "exactly MaterialRefresh's fields: {keys:?}");
}

fn gemini_login(refresh: &str, expiry_ms: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({"access_token":"offline-gemini-access",
        "refresh_token":refresh,"scope":"openid","token_type":"Bearer",
        "id_token":"offline-id","expiry_date":expiry_ms}))
    .unwrap()
}
async fn gemini_attempt(
    d: &Dirs,
    service: &mut FakeServer,
    rewrite: Option<Rewrite>,
) -> SubscriptionLogin {
    let state = d.home.join(".gemini");
    std::fs::create_dir(&d.home).unwrap();
    let mut login = SubscriptionLogin::start(service, LoginKind::GeminiOauthCreds, Some(&state))
        .await
        .unwrap();
    assert_eq!(login.installed().gemini_state(), Some(state.as_path()));
    assert!(login.installed().claude_env().is_none() && login.installed().codex_home().is_none());
    let creds = std::fs::metadata(state.join(GEMINI_OAUTH_FILE)).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&creds.permissions()) & 0o777,
        0o600
    );
    let mut harness = FakeHarness::new(&[HarnessExit::Completed]);
    harness.rewrite = rewrite;
    login.run(service, &mut harness).await.unwrap();
    login
}
fn gemini_refresh_to(bytes: Vec<u8>) -> Option<Rewrite> {
    Some(Box::new(move |state: &Path| {
        std::fs::write(state.join(GEMINI_OAUTH_FILE), &bytes).unwrap();
        std::fs::write(state.join("google_accounts.json"), b"{}").unwrap();
        std::fs::create_dir_all(state.join("tmp/w/chats")).unwrap();
        std::fs::write(state.join("tmp/w/chats/session.jsonl"), b"{}\n").unwrap();
    }))
}

#[tokio::test]
async fn gemini_refreshed_creds_written_back_and_only_login_files_wiped() {
    let d = dirs();
    let original = gemini_login("g-refresh-3", 1_800_000_000_000);
    let refreshed = gemini_login("g-refresh-3", 1_800_000_360_000);
    let mut service = server(vec![issued(LoginKind::GeminiOauthCreds, 3, &original)]);
    service.answers.push_back(Ok(WriteBack::Accepted {
        version: 4,
        replayed: false,
    }));
    let login = gemini_attempt(&d, &mut service, gemini_refresh_to(refreshed.clone())).await;
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(service.uploads, vec![(LEASE, 3, refreshed.clone())]);
    assert_eq!(receipt.outcome, ReceiptOutcome::Accepted);
    assert_eq!(journaled(&d)["kind"], "gemini_oauth_creds");
    let state = d.home.join(".gemini");
    assert!(!state.join(GEMINI_OAUTH_FILE).exists());
    assert!(!state.join("google_accounts.json").exists());
    assert!(
        state.join("tmp/w/chats/session.jsonl").exists(),
        "recording kept for the reader"
    );
    let raw = std::fs::read_to_string(d.journal.join("login-writeback.json")).unwrap();
    assert!(!raw.contains("g-refresh-3") && !raw.contains("offline-gemini-access"));
}

#[tokio::test]
async fn gemini_equal_older_or_invalid_creds_are_not_uploaded() {
    let original = gemini_login("g-refresh-3", 1_800_000_000_000);
    for rewrite in [
        None,
        gemini_refresh_to(gemini_login("g-other", 1_800_000_000_000)),
        gemini_refresh_to(gemini_login("g-other", 1_700_000_000_000)),
        gemini_refresh_to(br#"{"access_token":"a","expiry_date":1900000000000}"#.to_vec()),
        gemini_refresh_to(b"[]".to_vec()),
    ] {
        let d = dirs();
        let mut service = server(vec![issued(LoginKind::GeminiOauthCreds, 3, &original)]);
        let login = gemini_attempt(&d, &mut service, rewrite).await;
        let receipt = login
            .finish(&mut service, &d.journal, &d.files, soon())
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
        assert!(service.uploads.is_empty());
        assert!(!d.home.join(".gemini").join(GEMINI_OAUTH_FILE).exists());
    }
}

#[tokio::test]
async fn gemini_login_without_refresh_token_is_refused_at_install() {
    let d = dirs();
    let mut service = server(vec![issued(
        LoginKind::GeminiOauthCreds,
        1,
        br#"{"access_token":"only-access"}"#,
    )]);
    let state = d.home.join(".gemini");
    assert!(
        SubscriptionLogin::start(&mut service, LoginKind::GeminiOauthCreds, Some(&state))
            .await
            .is_err()
    );
    assert!(!state.exists());
}

fn opencode_login(refresh: &str, expires: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({"openai":{"type":"oauth","refresh":refresh,
        "access":"offline-access","expires":expires,"accountId":"offline-account"}}))
    .unwrap()
}
async fn opencode_attempt(
    d: &Dirs,
    service: &mut FakeServer,
    rewrite: Option<Rewrite>,
) -> SubscriptionLogin {
    let mut login =
        SubscriptionLogin::start(service, LoginKind::OpenCodeChatgptAuth, Some(&d.home))
            .await
            .unwrap();
    assert_eq!(
        login.installed().opencode_data_dir(),
        Some(d.home.as_path())
    );
    let mode = |p: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions())
            & 0o777
    };
    assert_eq!(mode(&d.home), 0o700);
    assert_eq!(mode(&d.home.join("auth.json")), 0o600);
    let mut harness = FakeHarness::new(&[HarnessExit::Completed]);
    harness.rewrite = rewrite;
    assert_eq!(
        login.run(service, &mut harness).await.unwrap(),
        AttemptOutcome::Exited(HarnessExit::Completed)
    );
    login
}

#[tokio::test]
async fn opencode_refreshed_login_written_back_and_only_the_login_wiped() {
    let d = dirs();
    let original = opencode_login("refresh-v7", 1_793_000_000_000);
    let refreshed = opencode_login("refresh-v8", 1_793_900_000_000);
    let mut service = server(vec![issued(LoginKind::OpenCodeChatgptAuth, 7, &original)]);
    service.answers.push_back(Ok(WriteBack::Accepted {
        version: 8,
        replayed: false,
    }));
    let login = opencode_attempt(&d, &mut service, refresh_to(refreshed.clone())).await;
    std::fs::write(d.home.join("opencode.db"), b"session store").unwrap();
    let receipt = login
        .finish(&mut service, &d.journal, &d.files, soon())
        .await
        .unwrap();
    assert_eq!(service.uploads, vec![(LEASE, 7, refreshed.clone())]);
    assert_eq!(receipt.outcome, ReceiptOutcome::Accepted);
    assert_eq!(receipt.version, Some(8));
    assert!(!d.home.join("auth.json").exists(), "login wiped");
    assert!(
        d.home.join("opencode.db").exists(),
        "transcript store left for the reader"
    );
    let raw = std::fs::read_to_string(d.journal.join("login-writeback.json")).unwrap();
    assert!(!raw.contains("refresh-v8") && !raw.contains("offline-access"));
}

#[tokio::test]
async fn opencode_unchanged_stale_or_foreign_login_is_not_uploaded() {
    for rewrite in [
        None,
        // Different bytes, older expiry: not a refresh.
        refresh_to(opencode_login("refresh-other", 1_700_000_000_000)),
        // A second provider credential is never accepted back.
        refresh_to(
            serde_json::to_vec(&json!({"openai":{"type":"oauth","refresh":"r","access":"a",
                "expires":1_799_000_000_000_i64},"anthropic":{"type":"api","key":"k"}}))
            .unwrap(),
        ),
    ] {
        let d = dirs();
        let original = opencode_login("refresh-v7", 1_793_000_000_000);
        let mut service = server(vec![issued(LoginKind::OpenCodeChatgptAuth, 7, &original)]);
        let login = opencode_attempt(&d, &mut service, rewrite).await;
        let receipt = login
            .finish(&mut service, &d.journal, &d.files, soon())
            .await
            .unwrap();
        assert_eq!(receipt.outcome, ReceiptOutcome::NotUploaded);
        assert!(service.uploads.is_empty() && !d.home.join("auth.json").exists());
    }
}

#[tokio::test]
async fn opencode_login_install_refuses_codex_or_api_key_material() {
    for material in [
        codex_login("refresh-v7", "2026-11-01T00:00:00Z"),
        serde_json::to_vec(&json!({"openai":{"type":"api","key":"sk-x"}})).unwrap(),
    ] {
        let d = dirs();
        let mut service = server(vec![issued(LoginKind::OpenCodeChatgptAuth, 7, &material)]);
        assert!(
            SubscriptionLogin::start(&mut service, LoginKind::OpenCodeChatgptAuth, Some(&d.home))
                .await
                .is_err()
        );
        assert!(!d.home.join("auth.json").exists());
    }
}

#[test]
fn login_kind_wire_names() {
    for (kind, name) in [
        (LoginKind::ClaudeOauthToken, "claude_oauth_token"),
        (LoginKind::CodexChatgptAuth, "codex_chatgpt_auth"),
        (LoginKind::GeminiOauthCreds, "gemini_oauth_creds"),
        (LoginKind::OpenCodeChatgptAuth, "opencode_chatgpt_auth"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), name);
    }
}
