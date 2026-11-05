# C6b — Runner subscription-login install, refresh write-back and wipe

Task: `calm-delta-18.10` (owner rescope of 2026-11-04). Plan: `docs/superpowers/plans/2026-11-04-C6b-subscription-login.md`. The plan, the implementation and this evidence are in one squashed commit, the one that contains this document. The AQ task close records the exact published OID.

**No real-server round-trip was run.** C5b's write-back endpoint lives on aq-server branch `aq/calm-delta-18.8`, which is not merged. Instead, the tests drive a fake `LoginService` and a recording axum peer that speaks C5b's wire contract. The MS1 demo exercises the path end to end.

## Delivered

### Attempt driver (`crates/aq-worker/src/native/runner/sandbox/subscription_login.rs`)

This is a library API that follows the C6a/B7 pattern. `calm-delta-18.10.1` wires it into launch.

- **Traits:**
  - **`LoginService` (line 84)** has two methods:
    - `fetch_newest(kind)` returns an `IssuedLogin { kind, lease_id, version, material }`;
    - `write_back(lease_id, base_version, bytes)` returns a `WriteBack`, which is one of `Accepted { version, replayed }`, `Superseded { version }` or `NotRenewable`. An `Err` means a transient failure.
  - **`Harness` (line 104)** launches the harness once and reports a `HarnessExit`: `Completed`, `AuthenticationFailed` or `Failed`.
- **Install (`install`, line 195):**
  - **Claude:** the token is kept only in memory as `Zeroizing<String>`. It must be UTF-8, non-empty, at most 4096 characters and ASCII graphic. `InstalledLogin::claude_env()` (line 163) returns `("CLAUDE_CODE_OAUTH_TOKEN", token)`. The token is never written to disk and never put in argv. The launcher adds it to the spawned process's env only; C6a's recipe still forbids that variable.
  - **Codex:** the login is checked with `validate_chatgpt_auth_json`, then written to `CODEX_HOME/auth.json` with mode 0600 using `create_new` and `O_NOFOLLOW` (`harness_config::write_private`). The Codex mapper already sets `cli_auth_credentials_store="file"` (`aq-codex-mapper/src/lib.rs:213,267`).
- **`SubscriptionLogin::start` (line 268)** fetches the newest login and installs it.
- **`SubscriptionLogin::run` (line 285)** launches the harness.
  - **On `AuthenticationFailed`:** it fetches the newest login once.
    - If that login is strictly newer and the same kind, it reinstalls and relaunches exactly once.
    - Otherwise, or if the relaunch also fails authentication, it returns `AttemptOutcome::Failed(LoginFailure { reason: credential_invalid, retryable: false })` and does not relaunch.
- **`SubscriptionLogin::finish` (line 314)** must be called on every teardown path.
  1. **Reconcile (`refreshed`, line 379).** It reads `auth.json` with `O_NOFOLLOW`, bounded to 64 KiB. The file counts as refreshed only if all of these hold:
     - its digest differs from the installed copy's;
     - it is still a valid ChatGPT login;
     - its RFC 3339 `last_refresh` is newer than the installed one's.
  2. **Upload (`upload`, line 401).** It sends "N+1 based on N": `base_version` is the version the login was issued at. Transient errors are retried with backoff from 50 ms to 2 s until the attempt deadline (`timeout_at`).
  3. **Handle the answer.**
     - `Superseded`: our copy is discarded.
     - `NotRenewable`, or the deadline ran out: the copy is kept only in runner-only state, as `RunnerFiles::write_secret("refreshed-login.json")`. It is never dropped silently.
  4. **Wipe (`wipe`, line 423).** It always runs `remove_dir_all` on `CODEX_HOME`. The Claude token is zeroized on drop.
  5. **Journal.** It writes a `WriteBackReceipt` to `<journal>/login-writeback.json` with `durable_new`. The receipt holds only the kind, lease, base version, outcome, digest, server version and whether a copy was retained. It never contains login bytes.
- **Redaction.** `Debug` for `IssuedLogin` and `InstalledLogin` prints `<redacted>` in place of the material.

### C5b write-back over `RunnerHttp` (`crates/aq-worker/src/native/runner/sandbox/auth.rs`)

- **`RunnerRoute::MaterialRefresh` (line 50).** Its path is `/v1/worker/profiled/material/refresh`. It is authorized as the same operation as `Material`: `FetchPreparationMaterial` in preparation scope, `FetchExecutionMaterial` otherwise.
- **`RunnerHttp::refresh_login(lease_id, base_version, bytes)` (line 561)** posts an `application/octet-stream` body.
  - **Header `aq-material-refresh`:** base64url-no-pad JSON with exactly these keys:
    - `schema_version: 1` and `operation_id`;
    - `identity { node_id, boot_id }`, taken from the permit binding;
    - `binding`, `lease_id`, `base_version` and `content_digest`.
    
    The encoded header is at most 8192 bytes.
  - **Responses:**
    - 200 `{accepted:true}` maps to `Accepted`;
    - 409 `{accepted:false, version}` maps to `Superseded`;
    - any other 409 maps to `NotRenewable`;
    - every other status goes through `status()`, which drops authority on a definitive refusal. A 5xx, 408 or 429 is a transient `Err`.

### Supporting changes

- **`harness_config.rs:29,42`:** `create_private_home` and `write_private` are now `pub(super)`.
- **`private.rs:30`:** `write_secret` now allows `refreshed-login.json`.
- **`sandbox.rs`:** declares `subscription_login` and its tests.

## Acceptance map

The tests are in `crates/aq-worker/src/native/runner/sandbox/subscription_login_tests.rs`.

| Requirement | Test |
| --- | --- |
| Auth failure stops with `credential_invalid`, non-retryable, no relaunch; retry once only with a newer version (t1) | `expired_subscription_login_fails_fast_with_credential_reason` (line 123) |
| Refreshed login is written back as N+1 based on N before teardown, then wiped; the journal holds no token (t2) | `refreshed_login_written_back_before_teardown` (line 214) |
| An unchanged or older `last_refresh` is not uploaded | `unchanged_or_stale_login_is_not_uploaded` (line 251) |
| A truncated or invalid refreshed file is not uploaded and is still wiped | `invalid_refreshed_login_is_not_uploaded` (line 275) |
| A symlinked `auth.json` is not followed and is still wiped | `reconcile_refuses_symlinked_login_and_still_wipes` (line 294) |
| On a server-newer version, our copy is discarded and not retained | `superseded_write_back_discards_refreshed_login` (line 315) |
| A failed write-back (not renewable, or out of time) keeps a runner-only copy | `failed_write_back_retains_runner_only_copy` (line 336) |
| `Debug` never shows the material | `login_debug_redacts_material` (line 361) |
| Wire contract: path, header, body, response mapping, authority kept | `runner_write_back_speaks_c5b_contract` (line 367) |

## Verification

Environment: WSL2 Linux 6.18, Rust 2024 toolchain from `~/.cargo/bin`, `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2`.

| Command | Result | Log |
| --- | --- | --- |
| `cargo test --locked --workspace expired_subscription_login_fails_fast_with_credential_reason` | 1 passed | `C6b-evidence/expired_subscription_login_fails_fast_with_credential_reason.log` |
| `cargo test --locked --workspace refreshed_login_written_back_before_teardown` | 1 passed | `C6b-evidence/refreshed_login_written_back_before_teardown.log` |
| `cargo test --locked -p aq-worker --lib -- subscription_login sandbox::auth sandbox::private sandbox::harness_config` | 15 passed | `C6b-evidence/module-tests.log` |
| Mutation: relaunch with no newer version (`\|\| false`), skip reconcile/upload (`if false &&`), then run both named tests | both FAILED (source restored with `git checkout`) | `C6b-evidence/red-mutation.log` |
| `cargo clippy --locked -p aq-worker --all-targets -- -D warnings` | clean | `C6b-evidence/clippy.log` |
| `rustfmt --edition 2024 --check` on the six touched files | clean | — |

## Adversarial review

The task is not marked for one. The task's J4 review covers the Codex `auth.json` exposure noted below.

## Limits

- **No real-server round-trip.** C5b is unmerged. The wire contract is checked against a recording peer built from C5b's `login_refresh/api.rs` on `aq/calm-delta-18.8`.
- **Launch wiring (`calm-delta-18.10.1`).** `sandbox::run_with_login` calls `start_as` before the spawn and `finish` on every post-launch teardown path before any stopped/preserved record, retention or finished outcome. The tests are `sandbox_runner` `login::*`. These limits remain:
  - The Claude token is added to the adapter's `LocalCommand.environment`. That environment is sealed in the runner-private host `launch.json`, so the token is at rest in runner-only state for the attempt. Exec-time-only delivery is a follow-up.
  - The sandbox launches once. An `AuthenticationFailed` exit, decided by the caller-supplied `classify`, is final: `RunOutcome::LoginFailed(credential_invalid)`. `SubscriptionLogin::run`'s relaunch is not used. No adapter emits an authentication-failure signal yet.
  - A subscription login and a C6a private input cannot be used together, because the host pins `CODEX_HOME` for private inputs.
  - Separated-user installs use per-thread `setresuid`/`setresgid` (`as_owner`). This path is untested here, because it needs the runner capabilities and accounts.
  - No production caller constructs `RunnerLogins` yet. It needs the material fetch and the issued version, which is the aq-server follow-up.
- **No runner `fetch_newest` implementation.** There is no runner material fetch for subscription logins yet, and C5b does not deliver the issued version with the material. The version is therefore explicit input and is never defaulted. A cross-repo aq-server follow-up is recorded on `calm-delta-18.10`.
- **The Codex `auth.json` is readable by the agent.** The CLI must read the file itself, so the `runner`/`agent` separation does **not** protect it. The controls are lease scope, one attempt per sandbox, the wipe and the J4 review.
- **`credential_invalid` is a runner-local reason.** There is no protocol schema change. Carrying it in the session result is part of launch wiring.
- **The home is created by path.** `O_NOFOLLOW` covers only the final component, as in C6a's limits.
- **Not in scope:** device auth, lease caps and Claude write-back. The Claude token is env-only and is never refreshed by the runner.

## Follow-ups

- Cross-repo (aq-server), recorded as a comment on `calm-delta-18.10`: deliver the issued login version with the leased material, and confirm the `aq-runner-channel` requirement for the refresh route.
