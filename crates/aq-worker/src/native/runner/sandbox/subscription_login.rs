//! Versioned subscription login for one sandbox attempt (C6b; runner side of
//! C5b's compare-and-swap login store).
//!
//! Before the attempt the runner fetches the newest leased login and installs
//! it: a Claude token goes only into the spawn environment as
//! `CLAUDE_CODE_OAUTH_TOKEN` (I4a; never argv, never the C6a recipe env, never
//! a file); a Codex ChatGPT `auth.json` goes into a fresh private `CODEX_HOME`
//! (the mapper already sets `cli_auth_credentials_store="file"`). The agent
//! user can read that file: runner/agent separation does not protect it.
//!
//! On a mid-run authentication failure the runner fetches the newest version
//! and relaunches once if it is newer; otherwise the attempt fails fast with
//! `credential_invalid`, not retryable. After the attempt, a Codex login whose
//! `last_refresh` advanced is written back as "N+1 based on N" before
//! teardown; a newer server version wins and ours is discarded. The login is
//! then wiped. Only a write-back that failed definitively or ran out of time
//! keeps a copy, in runner-only state.
//!
//! A Gemini CLI login is the google-auth-library `oauth_creds.json` in the
//! harness state directory (`$GEMINI_CLI_HOME/.gemini`, file storage forced);
//! its write-back stamp is `expiry_date`, which advances on every refresh.
//! Only the login files are wiped there, so the session recording survives
//! for the transcript reader.
//!
//! These are library APIs; B6a2 owns wiring them into launch.
use super::harness_config::{create_private_home, write_private};
use super::private::RunnerFiles;
use anyhow::{Context, Result, bail, ensure};
use aq_session_host::PrivateLaunchInput;
use aq_session_protocol::Digest;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;
use uuid::Uuid;
use zeroize::Zeroizing;

/// C5b's material store bound; also the write-back body bound.
pub const MAX_LOGIN_BYTES: usize = 64 * 1024;
const AUTH_FILE: &str = "auth.json";
pub const GEMINI_OAUTH_FILE: &str = "oauth_creds.json";
/// Files the Gemini CLI writes beside the login; wiped with it.
const GEMINI_LOGIN_FILES: [&str; 2] = [GEMINI_OAUTH_FILE, "google_accounts.json"];
pub const CLAUDE_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const JOURNAL_RECEIPT: &str = "login-writeback.json";
pub const RETAINED_FILE: &str = "refreshed-login.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginKind {
    ClaudeOauthToken,
    CodexChatgptAuth,
    GeminiOauthCreds,
}

/// One leased login version as issued by the server. `version` is the
/// server's version of these bytes (N); it is never guessed.
pub struct IssuedLogin {
    pub kind: LoginKind,
    pub lease_id: Uuid,
    pub version: i64,
    pub material: Zeroizing<Vec<u8>>,
}
impl std::fmt::Debug for IssuedLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedLogin")
            .field("kind", &self.kind)
            .field("lease_id", &self.lease_id)
            .field("version", &self.version)
            .field("material", &"<redacted>")
            .finish()
    }
}

/// C5b's answer to "version N+1 based on N".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBack {
    /// 200: stored as `version` (or an identical earlier upload, `replayed`).
    Accepted { version: i64, replayed: bool },
    /// 409 with an outcome: a newer version won. Discard ours; do not retry.
    Superseded { version: i64 },
    /// 409 without an outcome: the lease can no longer be renewed.
    NotRenewable,
}

/// The lease source and C5b write-back. An `Err` from `write_back` is
/// transient and is retried until the attempt deadline.
#[allow(async_fn_in_trait)]
pub trait LoginService {
    async fn fetch_newest(&mut self, kind: LoginKind) -> Result<IssuedLogin>;
    async fn write_back(
        &mut self,
        lease_id: Uuid,
        base_version: i64,
        bytes: &[u8],
    ) -> Result<WriteBack>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessExit {
    Completed,
    /// The harness reported an authentication failure (HTTP 401).
    AuthenticationFailed,
    Failed,
}

/// Launches the harness once with the installed login and reports its exit.
#[allow(async_fn_in_trait)]
pub trait Harness {
    async fn launch(&mut self, login: &InstalledLogin) -> Result<HarnessExit>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    CredentialInvalid,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginFailure {
    pub reason: FailureReason,
    pub retryable: bool,
}
impl LoginFailure {
    pub const CREDENTIAL_INVALID: Self = Self {
        reason: FailureReason::CredentialInvalid,
        retryable: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Exited(HarnessExit),
    Failed(LoginFailure),
}

enum Secret {
    Claude(Zeroizing<String>),
    Codex(PathBuf),
    /// The Gemini state directory holding `oauth_creds.json`.
    Gemini(PathBuf),
}

/// What a refresh advances: Codex `last_refresh` (RFC 3339) or Gemini
/// `expiry_date` (epoch milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stamp {
    None,
    Rfc3339(Option<String>),
    ExpiryMs(Option<i64>),
}

/// The login currently installed for the attempt.
pub struct InstalledLogin {
    kind: LoginKind,
    lease_id: Uuid,
    version: i64,
    digest: Digest,
    stamp: Stamp,
    secret: Secret,
}
impl std::fmt::Debug for InstalledLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledLogin")
            .field("kind", &self.kind)
            .field("lease_id", &self.lease_id)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
impl InstalledLogin {
    pub fn kind(&self) -> LoginKind {
        self.kind
    }
    pub fn version(&self) -> i64 {
        self.version
    }
    /// The one spawn-environment entry for Claude; add it to the child's
    /// environment only, never to argv or the recipe.
    pub fn claude_env(&self) -> Option<(&'static str, &str)> {
        match &self.secret {
            Secret::Claude(token) => Some((CLAUDE_TOKEN_ENV, token.as_str())),
            _ => None,
        }
    }
    /// The private `CODEX_HOME` holding `auth.json`.
    pub fn codex_home(&self) -> Option<&Path> {
        match &self.secret {
            Secret::Codex(home) => Some(home),
            _ => None,
        }
    }
    /// The Gemini state directory (`$GEMINI_CLI_HOME/.gemini`) holding
    /// `oauth_creds.json`.
    pub fn gemini_state(&self) -> Option<&Path> {
        match &self.secret {
            Secret::Gemini(dir) => Some(dir),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct RefreshStamp {
    last_refresh: Option<String>,
}
fn last_refresh(bytes: &[u8]) -> Result<Option<String>> {
    PrivateLaunchInput::validate_chatgpt_auth_json(bytes)?;
    Ok(serde_json::from_slice::<RefreshStamp>(bytes)?.last_refresh)
}
#[derive(Deserialize)]
struct GeminiCreds {
    refresh_token: Option<String>,
    expiry_date: Option<i64>,
}
/// A file-stored Gemini OAuth login: a JSON object with a refresh token.
fn gemini_expiry(bytes: &[u8]) -> Result<Option<i64>> {
    ensure!(bytes.len() <= MAX_LOGIN_BYTES, "Gemini login byte bound");
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("Gemini login shape"))?;
    ensure!(value.is_object(), "Gemini login shape");
    let creds: GeminiCreds =
        serde_json::from_value(value).map_err(|_| anyhow::anyhow!("Gemini login shape"))?;
    ensure!(
        creds.refresh_token.is_some_and(|t| !t.is_empty()),
        "Gemini login requires a refresh token"
    );
    Ok(creds.expiry_date)
}
fn stamp(kind: LoginKind, bytes: &[u8]) -> Result<Stamp> {
    Ok(match kind {
        LoginKind::ClaudeOauthToken => Stamp::None,
        LoginKind::CodexChatgptAuth => Stamp::Rfc3339(last_refresh(bytes)?),
        LoginKind::GeminiOauthCreds => Stamp::ExpiryMs(gemini_expiry(bytes)?),
    })
}
fn stamp_newer(refreshed: &Stamp, installed: &Stamp) -> bool {
    match (refreshed, installed) {
        (Stamp::Rfc3339(new), Stamp::Rfc3339(old)) => newer(new.as_deref(), old.as_deref()),
        (Stamp::ExpiryMs(Some(new)), Stamp::ExpiryMs(old)) => old.is_none_or(|old| *new > old),
        _ => false,
    }
}
fn newer(refreshed: Option<&str>, installed: Option<&str>) -> bool {
    let parse = |v: &str| chrono::DateTime::parse_from_rfc3339(v).ok();
    match (refreshed.and_then(parse), installed) {
        (Some(_), None) => true,
        (Some(new), Some(old)) => parse(old).is_none_or(|old| new > old),
        (None, _) => false,
    }
}

fn install(issued: IssuedLogin, login_home: Option<&Path>, fresh: bool) -> Result<InstalledLogin> {
    let digest = Digest::of(&issued.material);
    let (secret, stamp) = match issued.kind {
        LoginKind::ClaudeOauthToken => {
            let token = std::str::from_utf8(&issued.material)
                .ok()
                .map(str::trim_end)
                .filter(|t| {
                    !t.is_empty() && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic())
                })
                .context("Claude subscription token shape")?;
            (
                Secret::Claude(Zeroizing::new(token.to_owned())),
                Stamp::None,
            )
        }
        LoginKind::CodexChatgptAuth => {
            ensure!(
                issued.material.len() <= MAX_LOGIN_BYTES,
                "Codex login byte bound"
            );
            let stamp = stamp(issued.kind, &issued.material)?;
            let home = login_home.context("Codex login requires a private CODEX_HOME")?;
            place(home, AUTH_FILE, &issued.material, fresh)?;
            (Secret::Codex(home.to_owned()), stamp)
        }
        LoginKind::GeminiOauthCreds => {
            let stamp = stamp(issued.kind, &issued.material)?;
            let dir = login_home.context("Gemini login requires a private state directory")?;
            place(dir, GEMINI_OAUTH_FILE, &issued.material, fresh)?;
            (Secret::Gemini(dir.to_owned()), stamp)
        }
    };
    Ok(InstalledLogin {
        kind: issued.kind,
        lease_id: issued.lease_id,
        version: issued.version,
        digest,
        stamp,
        secret,
    })
}

/// Write the login file into a fresh owner-only directory, or replace it in
/// the existing one on a relaunch.
fn place(dir: &Path, name: &str, bytes: &[u8], fresh: bool) -> Result<()> {
    if fresh {
        create_private_home(dir)?;
    } else {
        super::private::check_directory(dir)?;
        match fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    write_private(dir, name, bytes)
}

/// Durable, byte-free record of what happened to the login at teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteBackReceipt {
    pub kind: LoginKind,
    pub lease_id: Uuid,
    pub base_version: i64,
    pub outcome: ReceiptOutcome,
    pub content_digest: Option<Digest>,
    pub version: Option<i64>,
    pub retained: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptOutcome {
    /// No refresh to report (Claude, unchanged, not newer, invalid or unreadable).
    NotUploaded,
    Accepted,
    Replayed,
    Superseded,
    NotRenewable,
    DeadlineExceeded,
}

#[must_use = "finish() writes back and wipes the login"]
pub struct SubscriptionLogin {
    installed: InstalledLogin,
}
impl SubscriptionLogin {
    /// Fetch the newest leased login and install it. `login_home` (Codex
    /// `CODEX_HOME`, or the Gemini `$GEMINI_CLI_HOME/.gemini`) must not exist
    /// yet; it is created owner-only.
    pub async fn start(
        service: &mut impl LoginService,
        kind: LoginKind,
        login_home: Option<&Path>,
    ) -> Result<Self> {
        let issued = service.fetch_newest(kind).await?;
        ensure!(issued.kind == kind, "leased login kind changed");
        Ok(Self {
            installed: install(issued, login_home, true)?,
        })
    }
    pub fn installed(&self) -> &InstalledLogin {
        &self.installed
    }

    /// Run the harness. One authentication failure is retried only with a
    /// strictly newer version; otherwise fail fast with `credential_invalid`.
    pub async fn run(
        &mut self,
        service: &mut impl LoginService,
        harness: &mut impl Harness,
    ) -> Result<AttemptOutcome> {
        let mut retried = false;
        loop {
            match harness.launch(&self.installed).await? {
                HarnessExit::AuthenticationFailed if !retried => {
                    retried = true;
                    let newest = service.fetch_newest(self.installed.kind).await?;
                    if newest.kind != self.installed.kind
                        || newest.version <= self.installed.version
                    {
                        return Ok(AttemptOutcome::Failed(LoginFailure::CREDENTIAL_INVALID));
                    }
                    let home = self
                        .installed
                        .codex_home()
                        .or(self.installed.gemini_state())
                        .map(Path::to_owned);
                    self.installed = install(newest, home.as_deref(), false)?;
                }
                HarnessExit::AuthenticationFailed => {
                    return Ok(AttemptOutcome::Failed(LoginFailure::CREDENTIAL_INVALID));
                }
                exit => return Ok(AttemptOutcome::Exited(exit)),
            }
        }
    }

    /// Before any teardown: write a refreshed Codex login back, then wipe the
    /// login and its home, then journal the receipt into `journal`.
    pub async fn finish(
        self,
        service: &mut impl LoginService,
        journal: &Path,
        files: &RunnerFiles,
        deadline: Instant,
    ) -> Result<WriteBackReceipt> {
        let installed = self.installed;
        let mut receipt = WriteBackReceipt {
            kind: installed.kind,
            lease_id: installed.lease_id,
            base_version: installed.version,
            outcome: ReceiptOutcome::NotUploaded,
            content_digest: None,
            version: None,
            retained: false,
        };
        let mut failure = None;
        let login_file = match &installed.secret {
            Secret::Codex(home) => Some(home.join(AUTH_FILE)),
            Secret::Gemini(dir) => Some(dir.join(GEMINI_OAUTH_FILE)),
            Secret::Claude(_) => None,
        };
        if let Some(file) = &login_file
            && let Some(refreshed) = refreshed(file, &installed)
        {
            receipt.content_digest = Some(Digest::of(&refreshed));
            match upload(service, &installed, &refreshed, deadline).await {
                Some(WriteBack::Accepted { version, replayed }) => {
                    receipt.outcome = if replayed {
                        ReceiptOutcome::Replayed
                    } else {
                        ReceiptOutcome::Accepted
                    };
                    receipt.version = Some(version);
                }
                Some(WriteBack::Superseded { version }) => {
                    receipt.outcome = ReceiptOutcome::Superseded;
                    receipt.version = Some(version);
                }
                other => {
                    receipt.outcome = match other {
                        Some(_) => ReceiptOutcome::NotRenewable,
                        None => ReceiptOutcome::DeadlineExceeded,
                    };
                    // Never dropped silently: keep it only in runner-only state.
                    match files.write_secret(RETAINED_FILE, &refreshed) {
                        Ok(()) => receipt.retained = true,
                        Err(e) => failure = Some(e.context("retain refreshed login")),
                    }
                }
            }
        }
        if let Err(e) = wipe(installed) {
            failure.get_or_insert(e);
        }
        aq_session_host::durable_new(
            &journal.join(JOURNAL_RECEIPT),
            &serde_json::to_vec(&receipt)?,
        )?;
        match failure {
            Some(e) => Err(e),
            None => Ok(receipt),
        }
    }
}

/// The agent-visible login file, only when it is a valid login that differs
/// from the installed one and carries a newer refresh stamp. A symlink,
/// oversize or invalid file is never followed or uploaded.
fn refreshed(path: &Path, installed: &InstalledLogin) -> Option<Zeroizing<Vec<u8>>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Zeroizing::new(Vec::new());
    (&mut file)
        .take(MAX_LOGIN_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_LOGIN_BYTES || Digest::of(&bytes) == installed.digest {
        return None;
    }
    let new = stamp(installed.kind, &bytes).ok()?;
    stamp_newer(&new, &installed.stamp).then_some(bytes)
}

/// `Some` is a definitive server answer; `None` means the deadline passed.
async fn upload(
    service: &mut impl LoginService,
    installed: &InstalledLogin,
    bytes: &[u8],
    deadline: Instant,
) -> Option<WriteBack> {
    let mut backoff = Duration::from_millis(50);
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let call = service.write_back(installed.lease_id, installed.version, bytes);
        match tokio::time::timeout_at(deadline, call).await {
            Ok(Ok(answer)) => return Some(answer),
            Ok(Err(_)) => {}
            Err(_) => return None,
        }
        tokio::time::sleep_until(deadline.min(Instant::now() + backoff)).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

fn wipe(installed: InstalledLogin) -> Result<()> {
    match installed.secret {
        // Zeroizing clears the token on drop.
        Secret::Claude(token) => drop(token),
        Secret::Codex(home) => match fs::remove_dir_all(&home) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                bail!("wipe Codex login home: {e}")
            }
            _ => {}
        },
        // The state directory keeps the session recording for the reader.
        Secret::Gemini(dir) => {
            for name in GEMINI_LOGIN_FILES {
                match fs::remove_file(dir.join(name)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        bail!("wipe Gemini login: {e}")
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
