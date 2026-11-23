//! calm-delta-18.10.1: the C6b subscription login at sandbox launch level.
use super::{support::*, wait_file};
use aq_worker::native::runner::sandbox::{
    RunOptions, RunOutcome, RunnerTransport, SandboxLogin, TransportFuture,
    private::RunnerFiles,
    subscription_login::{
        HarnessExit, IssuedLogin, LoginFailure, LoginKind, LoginService, WriteBack,
    },
};
use std::path::PathBuf;
use uuid::Uuid;
use zeroize::Zeroizing;

const LEASE: Uuid = Uuid::from_u128(0x18101);
const TOKEN: &str = "sk-ant-oat01-fixture-subscription-token";

/// A fake lease source and C5b server. Each write-back records whether any
/// stopped/preserved record already existed: it must run before both.
struct Leases {
    issued: Option<IssuedLogin>,
    run_dir: PathBuf,
    uploads: Vec<(Uuid, i64, Vec<u8>, bool)>,
}
impl Leases {
    fn new(f: &Fixture, kind: LoginKind, material: &[u8]) -> Self {
        Self {
            issued: Some(IssuedLogin {
                kind,
                lease_id: LEASE,
                version: 7,
                material: Zeroizing::new(material.to_vec()),
            }),
            run_dir: f.dir.path().join("sandbox-run"),
            uploads: Vec::new(),
        }
    }
}
impl LoginService for Leases {
    async fn fetch_newest(&mut self, _: LoginKind) -> anyhow::Result<IssuedLogin> {
        self.issued
            .take()
            .ok_or_else(|| anyhow::anyhow!("no newer login"))
    }
    async fn write_back(
        &mut self,
        lease_id: Uuid,
        base_version: i64,
        bytes: &[u8],
    ) -> anyhow::Result<WriteBack> {
        let recorded = ["stopped.json", "preserved.json"]
            .iter()
            .any(|name| self.run_dir.join(name).exists());
        self.uploads
            .push((lease_id, base_version, bytes.to_vec(), recorded));
        Ok(WriteBack::Accepted {
            version: base_version + 1,
            replayed: false,
        })
    }
}
fn codex_auth(last_refresh: &str) -> Vec<u8> {
    serde_json::to_vec(
        &serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
        "tokens":{"id_token":"offline-id","access_token":"offline-access",
            "refresh_token":"offline-refresh","account_id":"offline-account"},
        "last_refresh":last_refresh}),
    )
    .unwrap()
}
fn receipt(f: &Fixture) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(f.dir.path().join("sandbox-run/login-writeback.json")).unwrap(),
    )
    .unwrap()
}
fn sealed(f: &Fixture) -> serde_json::Value {
    let path = f
        .dir
        .path()
        .join("sandbox-run/hosts")
        .join(f.frozen.launch_id.to_string())
        .join("launch.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn long_permit(f: &mut Fixture) {
    f.frozen.authority.hard_deadline_ms = 201_000;
    f.permit.hard_deadline_ms = 201_000;
    f.frozen.manifest.resources.wall_time_ms = 200_000;
    f.frozen.authority.grant.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.frozen.manifest).unwrap();
}

#[tokio::test]
async fn cancel_writes_back_refreshed_login_and_wipes_before_preserve() {
    let mut f = Fixture::new(true, 120_000);
    long_permit(&mut f);
    let home = f.dir.path().join("codex-home");
    let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
    let mut leases = Leases::new(
        &f,
        LoginKind::CodexChatgptAuth,
        &codex_auth("2026-11-01T00:00:00Z"),
    );
    let refreshed = codex_auth("2026-11-04T00:00:00Z");
    let (tx, rx) = tokio::sync::watch::channel(false);
    let cancel = async {
        wait_file(&f.local.workspace.join("descendant-counter")).await;
        // The harness refreshed its login in CODEX_HOME, then is cancelled.
        std::fs::write(home.join("auth.json"), &refreshed).unwrap();
        tx.send(true).unwrap();
    };
    let mut central = Central::new(Mode::Stall);
    let (outcome, ()) = tokio::join!(
        aq_worker::native::runner::sandbox::run_with_login(
            &f.worker,
            &f.frozen,
            f.permit.clone(),
            f.dir.path(),
            &f.local,
            &f.adapter,
            &mut central,
            rx,
            RunOptions::default(),
            None,
            SandboxLogin {
                service: &mut leases,
                kind: LoginKind::CodexChatgptAuth,
                codex_home: Some(home.clone()),
                files: &files,
                classify: |_| HarnessExit::Completed,
            },
        ),
        cancel
    );
    assert_eq!(
        outcome.unwrap(),
        RunOutcome::Preserved {
            reason: "cancelled",
            stop_proven: true
        }
    );
    // One write-back of N+1 based on N, before any stopped/preserved record.
    assert_eq!(leases.uploads.len(), 1);
    let (lease, base, bytes, recorded) = &leases.uploads[0];
    assert_eq!(
        (*lease, *base, bytes, *recorded),
        (LEASE, 7, &refreshed, false)
    );
    // Wiped, and the byte-free receipt journaled in runner-only state.
    assert!(!home.exists());
    let receipt = receipt(&f);
    assert_eq!(receipt["outcome"], "accepted");
    assert_eq!(receipt["version"], 8);
    assert!(f.dir.path().join("sandbox-run/preserved.json").exists());
    // CODEX_HOME is the spawn environment; the login is never in argv.
    let sealed = sealed(&f);
    assert_eq!(
        sealed["command"]["environment"]["CODEX_HOME"],
        home.to_str().unwrap()
    );
    assert!(!sealed.to_string().contains("offline-refresh"));
}

/// Asserts at retention (the disk being preserved) that the login is gone.
struct Retaining {
    central: Central,
    home: PathBuf,
    run_dir: PathBuf,
}
impl RunnerTransport for Retaining {
    fn preflight(&mut self, worker: &aq_worker::config::WorkerConfig) -> anyhow::Result<()> {
        self.central.preflight(worker)
    }
    fn renew<'a>(
        &'a mut self,
        worker: &'a aq_worker::config::WorkerConfig,
        request: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
        nonce: Uuid,
    ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::RenewalOutcome> {
        self.central.renew(worker, request, nonce)
    }
    fn retain<'a>(
        &'a mut self,
        worker: &'a aq_worker::config::WorkerConfig,
        stopped: &'a aq_worker::native::runner::sandbox::StoppedAttempt,
    ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::RetentionReceipts> {
        assert!(!self.home.exists(), "login must be wiped before preserve");
        assert!(self.run_dir.join("login-writeback.json").exists());
        self.central.retain(worker, stopped)
    }
    fn finished<'a>(
        &'a mut self,
        worker: &'a aq_worker::config::WorkerConfig,
        retained: &'a aq_worker::native::runner::sandbox::RetainedAttempt,
    ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::FinishedReceipt> {
        self.central.finished(worker, retained)
    }
}

#[tokio::test]
async fn completion_wipes_login_before_retention() {
    let f = Fixture::new(false, 5000);
    let home = f.dir.path().join("codex-home");
    let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
    let mut leases = Leases::new(
        &f,
        LoginKind::CodexChatgptAuth,
        &codex_auth("2026-11-01T00:00:00Z"),
    );
    let mut transport = Retaining {
        central: Central::new(Mode::Complete),
        home: home.clone(),
        run_dir: f.dir.path().join("sandbox-run"),
    };
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let outcome = aq_worker::native::runner::sandbox::run_with_login(
        &f.worker,
        &f.frozen,
        f.permit.clone(),
        f.dir.path(),
        &f.local,
        &f.adapter,
        &mut transport,
        rx,
        RunOptions::default(),
        None,
        SandboxLogin {
            service: &mut leases,
            kind: LoginKind::CodexChatgptAuth,
            codex_home: Some(home.clone()),
            files: &files,
            classify: |_| HarnessExit::Completed,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (outcome, transport.central.retentions),
        (RunOutcome::Finished, 1)
    );
    assert!(leases.uploads.is_empty(), "unchanged login is not uploaded");
    assert_eq!(receipt(&f)["outcome"], "not_uploaded");
}

#[tokio::test]
async fn claude_authentication_failure_is_final_credential_invalid() {
    let f = Fixture::new(false, 5000);
    let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
    let mut leases = Leases::new(&f, LoginKind::ClaudeOauthToken, TOKEN.as_bytes());
    let mut central = Central::new(Mode::Complete);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let outcome = aq_worker::native::runner::sandbox::run_with_login(
        &f.worker,
        &f.frozen,
        f.permit.clone(),
        f.dir.path(),
        &f.local,
        &f.adapter,
        &mut central,
        rx,
        RunOptions::default(),
        None,
        SandboxLogin {
            service: &mut leases,
            kind: LoginKind::ClaudeOauthToken,
            codex_home: None,
            files: &files,
            classify: |_| HarnessExit::AuthenticationFailed,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        RunOutcome::LoginFailed {
            failure: LoginFailure::CREDENTIAL_INVALID,
            stop_proven: true
        }
    );
    assert!(leases.uploads.is_empty());
    assert_eq!(central.retentions, 0, "a failed login attempt is preserved");
    assert!(f.dir.path().join("sandbox-run/login-failure.json").exists());
    assert_token_environment(&f);
}

fn assert_token_environment(f: &Fixture) {
    // The token is only the spawned process environment: not argv, not the
    // manifest recipe environment.
    let sealed = sealed(f);
    assert_eq!(
        sealed["command"]["environment"]["CLAUDE_CODE_OAUTH_TOKEN"],
        TOKEN
    );
    assert_eq!(sealed.to_string().matches(TOKEN).count(), 1);
}
