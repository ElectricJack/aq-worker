# C6b Subscription-Login Install, Write-Back and Wipe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The runner side of C5b's versioned subscription login: install the newest leased login before an attempt, retry once on a mid-run auth failure, write a refreshed Codex login back as "N+1 based on N" before teardown, and wipe it.

**Architecture:** One library module `runner/sandbox/subscription_login.rs` (like C6a/B7: B6a2 wires it into launch). It owns the login for one attempt through a `LoginService` trait (fetch newest, CAS write-back) and a `Harness` trait (launch once, report how it exited). `RunnerHttp` gains the C5b write-back route; a fake service drives the tests.

**Tech Stack:** Rust 2024, tokio, reqwest, serde, base64 (URL_SAFE_NO_PAD), zeroize.

**Spec:** task calm-delta-18.10 (owner rescope 2026-11-04 and operator comment on C5b's contract); C5b contract `aq-server crates/aq-server/src/profiled_execution/login_refresh/api.rs` (branch aq/calm-delta-18.8, not merged).

## Global Constraints

- Claude: env `CLAUDE_CODE_OAUTH_TOKEN` only, never argv, never logged, never in the C6a recipe env; no write-back.
- Codex: `CODEX_HOME/auth.json` 0600 (create_new, O_NOFOLLOW) with `cli_auth_credentials_store = "file"`.
- Write-back: `POST /v1/worker/profiled/material/refresh`, header `aq-material-refresh` = base64url-no-pad JSON `{schema_version:1, operation_id, identity{node_id,boot_id}, binding, lease_id, base_version, content_digest}`, octet-stream body ≤ 64 KiB.
- 200 `{accepted:true}` = accepted; 409 with `{accepted:false, version}` = superseded, discard, no retry; any other 409 = lease not renewable (definitive). Other failures are transient: retry until the attempt deadline.
- `base_version` is the version the login was issued at; never defaulted.
- No login on disk after teardown except a runner-only retained copy when write-back failed definitively or ran out of time.
- Auth failure with no newer version, or a second auth failure: reason `credential_invalid`, `retryable: false`, no relaunch.
- Out of scope: device auth, lease caps, server storage, F4, B4, I1–I3.

## Review Focus

- The agent replaces `auth.json` with a symlink or a huge file: reconcile must refuse to follow it and must still wipe (test `reconcile_refuses_symlinked_login_and_still_wipes`).
- The refreshed file is not a valid ChatGPT login (the agent truncated it): no upload, wipe (test `invalid_refreshed_login_is_not_uploaded`).
- The server answers superseded: the refreshed copy is discarded, not retained, no retry (test `superseded_write_back_discards_refreshed_login`).
- The write-back times out until the deadline: the copy is retained only in runner state (test `failed_write_back_retains_runner_only_copy`).
- Debug/Display of login types must not contain the material (test `login_debug_redacts_material`).

---

### Task 1: Subscription-login module (install, attempt driver, reconcile, wipe)

**Files:**
- Create: `crates/aq-worker/src/native/runner/sandbox/subscription_login.rs`
- Create: `crates/aq-worker/src/native/runner/sandbox/subscription_login_tests.rs`
- Modify: `crates/aq-worker/src/native/runner/sandbox.rs` (declare modules)
- Modify: `crates/aq-worker/src/native/runner/sandbox/private.rs` (allow `refreshed-login.json`)

**Interfaces (produces):**
- `enum LoginKind { ClaudeOauthToken, CodexChatgptAuth }`
- `struct IssuedLogin { kind, lease_id: Uuid, version: i64, material: Zeroizing<Vec<u8>> }` (redacting Debug)
- `enum WriteBack { Accepted { version: i64, replayed: bool }, Superseded { version: i64 }, NotRenewable }`
- `trait LoginService { async fn fetch_newest(&mut self, kind) -> Result<IssuedLogin>; async fn write_back(&mut self, lease_id, base_version, bytes: &[u8]) -> Result<WriteBack>; }` — `Err` is transient.
- `enum HarnessExit { Completed, AuthenticationFailed, Failed }`; `trait Harness { async fn launch(&mut self, login: &InstalledLogin) -> Result<HarnessExit>; }`
- `InstalledLogin::claude_env(&self) -> Option<(&'static str, &str)>` and `InstalledLogin::codex_store_override() -> [&'static str; 2]`.
- `SubscriptionLogin::start(service, kind, codex_home: Option<&Path>) -> Result<Self>`; `run(&mut self, service, harness) -> Result<AttemptOutcome>` where `AttemptOutcome::{Exited(HarnessExit), Failed(LoginFailure)}`; `finish(self, service, journal: &Path, files: &RunnerFiles, deadline: Instant) -> Result<WriteBackReceipt>`.
- `struct LoginFailure { reason: FailureReason::CredentialInvalid (serde "credential_invalid"), retryable: false }`.

- [ ] **Step 1: Write the failing named tests**
  - `expired_subscription_login_fails_fast_with_credential_reason`: Claude token, harness always returns `AuthenticationFailed`, service's newest is the same version. Assert outcome is `Failed(LoginFailure{reason: CredentialInvalid, retryable:false})`, serialized reason is `"credential_invalid"`, launches == 1, fetches == 2 (start + recheck). A second case where newest is newer and the retry also fails asserts launches == 2 and the second launch saw the newer token.
  - `refreshed_login_written_back_before_teardown`: Codex login version 7; the harness rewrites `auth.json` with a newer `last_refresh`. After `finish`, the service saw exactly one write-back with `base_version == 7` and the refreshed bytes; the journal has `login-writeback.json` with outcome accepted and version 8 and no token bytes; `CODEX_HOME` is gone; nothing is retained.
  - The Review Focus tests above.
- [ ] **Step 2: Run them and see them fail** — `cargo test --locked -p aq-worker --lib subscription_login` fails to compile (module missing).
- [ ] **Step 3: Implement the module.** Install writes `auth.json` after `validate_chatgpt_auth_json`. Reconcile reads with O_NOFOLLOW, bounded to 64 KiB, uploads only when the digest differs, it validates, and its `last_refresh` (RFC 3339) is newer than the installed one. Retry transient errors with backoff until `deadline`. Wipe with `remove_dir_all` of the home, and zeroize the token.
- [ ] **Step 4: Run the tests and see them pass.**
- [ ] **Step 5: Commit.**

### Task 2: C5b write-back over `RunnerHttp`

**Files:**
- Modify: `crates/aq-worker/src/native/runner/sandbox/auth.rs` (route `MaterialRefresh`, `refresh_login`)
- Test: `crates/aq-worker/src/native/runner/sandbox/subscription_login_tests.rs`

**Interfaces:** `RunnerHttp::refresh_login(&self, lease_id: Uuid, base_version: i64, bytes: &[u8]) -> Result<WriteBack>`; `RunnerRoute::MaterialRefresh` maps to the same operation as `Material`.

- [ ] **Step 1: Write `runner_write_back_speaks_c5b_contract`**: a recording peer answers 200 `{accepted:true,version:8,replayed:false}`, then 409 `{accepted:false,version:9,...}`, then 409 `{"error":..}`. Assert path, `content-type: application/octet-stream`, decoded header fields (`schema_version 1`, `base_version 7`, `content_digest == Digest::of(body)`, the node identity from the permit binding), and the three replies map to `Accepted{8,false}`, `Superseded{9}`, `NotRenewable`, with authority not lost.
- [ ] **Step 2: Run, see it fail.**
- [ ] **Step 3: Implement**: authorize as the Material route, post octet-stream with the header; 200 and 409 are handled here, and everything else goes through `status()`.
- [ ] **Step 4: Run, see it pass. Commit.**
