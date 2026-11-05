# B7 Scoped AQ Agent Access in Sandboxes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (native, pool worker) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent inside a sandbox reaches AQ only through `aq-agent`, using a per-attempt scoped credential, with the runner's loopback AQ proxy as its endpoint.

**Architecture:** The node issues the agent credential. It calls `/v2/worker/agent-session` through the existing `NodeClient`/`Allocator` and delivers the session files over the control channel. The runner never calls issuance.

Inside the sandbox, a new `aq_agent_client::provision::install_delivered` installs the files into the agent user's private state dir:
- It rewrites `server_url` to the B6b `AqApiProxy` origin.
- It refuses Access egress material.
- It caps the credential at the attempt grant.

A worker-side `AgentAccess` guard calls it. The guard stops provisioning and removes the credential on cancellation, expiry or revocation. `aq-agent` gains `message inbox|send|ack|body`, and the runtime bundle ships `/opt/aq/bin/aq-agent`.

**Tech Stack:** Rust 2024 (reqwest/rustls, axum test fakes), vendored `aq-agent-api` 0.1.0 schemas, Python packaging scripts, Docker.

**Spec:** the noble-summit-37.10 task description, `docs/specs/0002-execution-and-agent-access.md` §4, and aq-server `docs/architecture/0005-…` §4 step 3.

## Global Constraints
- The credential is never in argv, prompts, environment dumps or logs; errors never contain token bytes.
- Credential expiry must be ≤ the attempt grant's expiry and hard deadline; the server stays authoritative.
- The agent never holds the Access token. `edge_authentication` must be `false` in sandbox delivery, and `agent-egress.json` is never installed.
- Ship `aq-agent`, never `aq`, under `/opt/aq/bin`.
- Out of scope: mailbox poll/ack harness delivery, `message reply`, waits, supervisor credentials, base bundle build, proxy changes, server permit enforcement.
- Build bounds: `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2`, `--locked`.

## Review Focus
- A delivered credential whose `expires_at` is later than the grant must be refused, not clamped silently. This is covered in Task 2's test.
- A delivery whose binding names another attempt or node must be refused. This is covered in Task 2's test.
- A message recipient outside the bound task or session mailbox must be refused before send. This is covered in Task 1's test.
- Provisioning after `stop()` must fail even when the grant time has not yet passed. This is covered in Task 2's test.
- `client.json` must carry the proxy origin, and Access headers come only from the proxy. This is covered in Task 2's versioned-mutation test.

---

### Task 1: aq-agent messages + delivered installation (aq-agent-client)

**Files:**
- Modify: `crates/aq-agent-client/src/client.rs` (message methods)
- Modify: `crates/aq-agent-client/src/main.rs` (`Message` subcommand)
- Modify: `crates/aq-agent-client/src/provision.rs` (`SessionDelivery`, `install_delivered`, `revoke_delivered`, `Allocator::delivery`)
- Test: `crates/aq-agent-client/tests/client.rs`

**Interfaces (produces):**
- `pub enum MessageRoute { Send, Inbox, Ack, Body }`
- `AgentClient::message(&self, route: MessageRoute, request: Value) -> Result<Outcome>`. It validates the request and response schemas (`MessageSend`/`MessageSendReceipt`, `MessageInbox`/`MessageInboxResponse`, `MessageAck`/`MessageAckReceipt`, `MessageBodyRequest`/`MessageBodyResponse`). The recipient must equal the bound `TaskMailbox` or `SessionMailbox`.
- `AgentClient::inbox_request(&self, task_mailbox: bool, after: &str, limit: u8, thread: Option<Uuid>) -> Value`
- CLI: `aq-agent message inbox [--task] [--after N] [--limit N] [--thread UUID]` and `aq-agent message {send|ack|body} --request-file F`.
- `pub struct SessionDelivery { pub client: Vec<u8>, pub context: Vec<u8>, pub credential: Vec<u8> }`. Its `Debug` impl redacts the bytes.
- `Allocator::delivery(&self, session: Uuid) -> Result<SessionDelivery>` (reads the ready dir)
- `pub fn install_delivered(state_dir: &Path, delivery: &SessionDelivery, endpoint: &str, not_after_ms: u64, now_ms: u64) -> Result<Installed>`
- `pub struct Installed { pub path: PathBuf, pub binding: Binding, pub expires_at_ms: u64 }`
- `pub fn revoke_delivered(state_dir: &Path) -> Result<()>` (removes `credential.json`)

- [ ] Write a failing test `message_inbox_and_send_stay_in_bound_mailbox`. Its fake server answers `/v1/agent/messages/inbox` and `/send` with fixture-shaped receipts. It asserts that both calls exit 0 and that a send to a foreign task mailbox is refused before any request.
- [ ] Implement it, then run `cargo test --locked -p aq-agent-client --test client message_` and confirm it passes.

### Task 2: worker AgentAccess guard + named acceptance tests (aq-worker)

**Files:**
- Modify: `crates/aq-worker/Cargo.toml` (adds `aq-agent-client = { path = "../aq-agent-client", version = "=0.1.0" }`)
- Create: `crates/aq-worker/src/native/runner/sandbox/agent_access.rs`, with `pub mod agent_access;` in `sandbox.rs`
- Test: `crates/aq-worker/tests/agent_access.rs`

**Interfaces:**
- `pub struct AgentGrant { pub attempt_id: Uuid, pub node_id: Uuid, pub not_after_ms: u64, pub live: bool }`
- `AgentGrant::from_runner(http: &RunnerHttp) -> Self`. Its `not_after_ms` is `min(grant_expires_at_ms, hard_deadline_ms)`, and `live = !authority_lost()`.
- `pub struct AgentAccess`
- `AgentAccess::new(state_dir: PathBuf, proxy_origin: &str) -> Self`
- `AgentAccess::provision(&self, delivery: &SessionDelivery, grant: &AgentGrant, now_ms: u64) -> Result<Installed>`. It refuses when stopped, when the grant is not live, when `now_ms >= not_after_ms`, or when the binding attempt or node differs. It then calls `install_delivered(..., not_after_ms, now_ms)`.
- `AgentAccess::stop(&self) -> Result<()>`. It sets the stopped flag permanently and calls `revoke_delivered`.

- [ ] Write the failing test `agent_credential_expires_with_attempt`. It asserts:
  - a credential expiring after the grant is refused;
  - an in-grant credential installs, and its `expires_at_ms ≤ not_after_ms`;
  - after `stop()` the credential file is gone, and `provision` is refused;
  - a grant with `now ≥ not_after` or `live=false` is refused;
  - `AgentClient` refuses a credential past `expires_at` with exit 14.
- [ ] Write the failing test `agent_reads_scoped_context_and_writes_versioned_mutation`. It sets up a fake AQ upstream on a raw loopback socket, then `AqApiProxy::development_loopback` and `AgentAccess::provision`. It runs the built `aq-agent` binary (`--state-dir`) for `context` and `graph command --request-file`, with a CommandRequest carrying `expected_version`. It asserts:
  - the upstream saw `Authorization: Bearer aqa_…`, the cf-access headers and `expected_version`;
  - stdout and stderr contain neither the Access secret nor the agent token;
  - `client.json` `server_url` equals the proxy origin.
- [ ] Implement it, then run both named tests and confirm they pass.

### Task 3: runtime bundle ships aq-agent

**Files:** `packaging/sandbox/Dockerfile.runtime`, `packaging/sandbox/bundle_manifest.py`, `scripts/package-sandbox.py`, `packaging/sandbox/README.md`, `crates/aq-worker/tests/runtime_bundle.rs`, `scripts/tests/test_package_sandbox.py` (if it asserts the artifact list).

- [ ] Update the runtime_bundle test so that `bin/aq-agent` is expected in the manifest (`agent_client` artifact), and so that no `bin/aq` exists. Confirm it fails.
- [ ] Make these changes, then confirm `cargo test --locked -p aq-worker --test runtime_bundle` and `python3 -m pytest scripts/tests/test_package_sandbox.py` pass:
  - Build `-p aq-agent-client --bin aq-agent` and copy it to `/opt/aq/bin/aq-agent`, with a `--version` check.
  - Add the manifest entry `agent_client`.
  - Add `agent_client_version` to source.json from `crates/aq-agent-client/Cargo.toml`.
  - Fix the README sentence.

### Task 4: DoD + evidence
- [ ] Run `cargo fmt --check` and `cargo clippy --locked -p aq-agent-client -p aq-worker --all-targets -- -D warnings`, plus both named workspace tests.
- [ ] Write `docs/implementation/B7.md`: decision, commands, results, commit, limits. One limit to record: installation must run as the agent user (`run_as_agent`), because `PrivateDir` requires euid ownership.
