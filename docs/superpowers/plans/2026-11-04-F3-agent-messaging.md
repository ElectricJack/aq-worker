# F3 Agent Messaging in Sandboxes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents in sandboxes receive AQ mail exactly once, delivered at a safe boundary or to their inbox, and can read, send and reply with `aq-agent message ...`.

**Architecture:**
- A runner mail loop (`sandbox/mail.rs`) polls `/v2/worker/messages/poll` through the existing permit-only `RunnerHttp`.
  - It journals each `delivery_id` durably before any effect.
  - It hands the delivery to an inbox sink or to a qualified safe-boundary hook (`SafeBoundary`, which H3 consumes).
  - It acks `/v2/worker/messages/ack` with the journaled disposition.
- `aq-agent` gains `message reply` and `message body --message-id`, with chunk following.
- The supervisor recipient and sender that agent-access v1 lacks are pinned locally (`aq_agent_client::mail`) as an additive extension over vendored 0.1.0. The server change is a CROSS-REPO FOLLOW-UP (aq-server).

**Tech Stack:** Rust 2024, tokio, reqwest, axum (tests only), serde_json, vendored `aq-agent-api` 0.1.0 schema.

**Spec:** task steady-harbor-87.4 description; aq-server/docs/operations/messaging.md; docs/specs/0004 §2; docs/specs/0003 §3.

## Global Constraints
- Poll limit: at most 16 envelopes per response. Inbox pages: at most 32. Bodies: 1–16,384 UTF-8 bytes.
- Ack dispositions are exactly one of `received_by_node`, `handed_to_harness`, `delivery_unknown` or `stale`.
- Never inject keystrokes or press Enter at an unknown prompt. Native delivery happens only through a qualified `SafeBoundary`.
- An uncertain injection is `delivery_unknown` and is never retried. A stopped or suspending attempt gets `stale` for new deliveries.
- Never print tokens. The agent never holds the Access token.
- Targeted tests only. No real sleeps or slow tests.

## Review Focus
- **Replay with a changed body or digest** under a journaled `delivery_id`: refuse it with an error and deliver nothing. Test `message_delivered_once_across_runner_restart` asserts this.
- **Crash between intent and outcome:** on restart, ack `delivery_unknown` and don't call the sink. Covered in the restart test.
- **Native boundary not ready (`Deferred`):** ack `received_by_node` only, then retry the hook on the next replay. Covered in Task 2's test.
- **Reply to an owner sender:** agent-access v1 has no owner mailbox. Refuse before any send. Covered in Task 1's test.
- **Reply target listed as an omitted entry** in the inbox page: re-read it with `limit=1` to recover the sender. Covered in Task 1's test.

---

### Task 1: Supervisor mail pin + `aq-agent message reply` / body chunks

**Files:**
- Create: `crates/aq-agent-client/src/mail.rs`
- Modify: `crates/aq-agent-client/src/lib.rs`, `crates/aq-agent-client/src/client.rs`, `crates/aq-agent-client/src/main.rs`
- Test: `crates/aq-agent-client/tests/client.rs`

**Interfaces:**
- Produces: `aq_agent_client::mail::valid(name: &str, value: &Value) -> bool`.
  - It accepts vendored-valid documents.
  - It also accepts documents where a `recipient` is `{"kind":"global_supervisor","conversation_id":uuid}`, or a `sender` is `{"kind":"central_supervisor","actor_id","generation":>=1,"session_id","conversation_id"}`. Both must be exact keys with non-nil UUIDs.
  - Implementation: substitute placeholder vendored-valid shapes, then run the vendored `valid`.
- Produces: `mail::reply_recipient(sender: &Value) -> Result<Value>`.
  - session → `SessionMailbox` from the sender's binding.
  - central_supervisor → `{"kind":"global_supervisor","conversation_id"}`.
  - owner → error.
- Produces: `mail::check_body(body: &str) -> Result<()>` for 1..=16384 bytes.
- Produces: `AgentClient::reply(&self, session: bool, message_id: Uuid, body: String, operation_id: Uuid, reply_id: Uuid) -> Result<Outcome>`.
  - It scans the bound inbox (at most 64 pages of 32) for `message_id`.
  - It builds a `MessageSend` on the original `thread_id`.
- Produces: `AgentClient::body(&self, session: bool, message_id: Uuid) -> Result<Outcome>`.
  - It follows `next_offset` until `complete`.
  - It requires the same `payload_digest` on every chunk and returns `{message_id, payload_digest, body}`.
- `message()` uses `mail::valid`. A send to a `global_supervisor` recipient passes the scope check, because the daemon authorizes it.

- [ ] **Step 1:** Write the failing test `agent_reply_reaches_supervisor_mailbox` (peer mode 20).
  - Setup: the inbox returns `mailbox-page` with `messages[0].sender` set to the central_supervisor sender (conversation C).
  - Assert one send whose `recipient == {"kind":"global_supervisor","conversation_id":C}`, whose `thread_id` matches the original, and whose body is the reply.
  - Assert the exit code is 0.
  - Also assert:
    - a session sender reply goes to that session's mailbox;
    - an owner sender reply is refused, with no send call;
    - an empty or 16,385-byte body is refused before the network.
- [ ] **Step 2:** `cargo test --locked -p aq-agent-client agent_reply_reaches_supervisor_mailbox` should FAIL to compile, because `reply` doesn't exist yet.
- [ ] **Step 3:** Implement `mail.rs`, `reply`, `body`, and the CLI wiring:
  - `Message::Reply {--message-id, --task, --body-file?, --operation-id?, --reply-id?}`. Without `--body-file`, the body comes from stdin.
  - `Message::Body {--request-file? | --message-id + --task}`.
- [ ] **Step 4:** Re-run the test: PASS. Also run `message_inbox_and_send_stay_in_bound_mailbox`: PASS.

### Task 2: Runner mail loop with exactly-once journal

**Files:**
- Create: `crates/aq-worker/src/native/runner/sandbox/mail.rs`
- Modify: `crates/aq-worker/src/native/runner/sandbox.rs` (`pub mod mail;`)
- Test: `crates/aq-worker/tests/sandbox_mail.rs`

**Interfaces:**
- Consumes: `RunnerHttp::post_bounded(RunnerRoute::MailPoll|MailAck, &binding, &req, max_req, max_resp)`, `RunnerHttp::launch_binding().session` (`SessionIdentity::Remote`), `aq_agent_client::mail::valid`, `aq_session_host::durable_new`.
- Produces:
  - `pub enum Injection { Handed, Deferred, Unknown }`
  - `pub trait SafeBoundary: Send { fn inject(&mut self, delivery: &Value) -> Injection; }`. This is the qualified adapter readiness/composer guard. H3 nudges use this hook.
  - `pub enum Delivery<'a> { Inbox, Native(&'a mut dyn SafeBoundary) }`
  - `pub enum AttemptInput { Accepting, Rejecting }`
  - `pub struct MailLoop` with:
    - `MailLoop::open(http: RunnerHttp, journal_dir: &Path) -> Result<Self>`
    - `async fn poll_once(&mut self, input: AttemptInput, delivery: Delivery<'_>) -> Result<Vec<(Uuid, &'static str)>>`, which returns the `(delivery_id, disposition)` pairs acked in this pass.
  - `pub fn MailLoop::record_intent(&self, delivery_id: Uuid, message_id: Uuid, digest: &str) -> Result<()>`, a public crash-simulation entry point. The loop uses it itself before calling the sink.
- Journal: in `journal_dir`, one create-new durable file per stage.
  - `<id>.received` holds `{message_id, payload_digest, ack_op}`.
  - `<id>.intent`.
  - `<id>.outcome` holds `{disposition, ack_op}`.
  - `<id>.acked` holds the acked disposition.
- Per-delivery algorithm:
  - Validate the batch: `mail::valid("MessageNodeBatch")`, the attempt equals the request attempt, and there are at most 16 deliveries.
  - Write the received record, or check it matches. A mismatch is an error.
  - If an outcome exists, ack it again if it isn't acked yet; otherwise do nothing.
  - Else if an intent exists, record the outcome `delivery_unknown`.
  - Else if `Rejecting`, record the outcome `stale`.
  - Else if `Inbox`, record the outcome `received_by_node`.
  - Else (`Native`):
    - `Deferred` → ack `received_by_node`, with no outcome recorded;
    - `Handed` → `handed_to_harness`;
    - `Unknown` → `delivery_unknown`.
    - Record the intent before calling the sink.
  - Ack the outcome with the journaled `ack_op`. Validate `MessageNodeAckReceipt` (echoes `delivery_id`, `operation_id`, disposition), then write `<id>.acked`.

- [ ] **Step 1:** Write the failing test `message_delivered_once_across_runner_restart`.
  - Uses a loopback axum fake for poll and ack, the `support::Fixture` permit, and `RunnerHttp::for_execution`.
  - Loop A (native, recording sink) sees D1 → sink called once, ack `handed_to_harness`.
  - Drop A, then open loop B on the same dir. The poll replays D1 (`received_by_node`) and adds D2 → the sink sees only D2, and D1 is not re-acked.
  - Pre-record an intent for D3 → `delivery_unknown`, with no sink call.
  - A third poll replays all three → no sink calls and no acks.
  - D1 replayed with a changed digest → `poll_once` errors and nothing is delivered.
  - Separate test `mail_inbox_mode_and_rejecting_attempt`:
    - inbox → `received_by_node` with no sink;
    - Rejecting → `stale`;
    - Native `Deferred` → `received_by_node`, then `Handed` on a later replay.
- [ ] **Step 2:** `cargo test --locked -p aq-worker --test sandbox_mail` should FAIL, because the module doesn't exist yet.
- [ ] **Step 3:** Implement `mail.rs` per the algorithm above.
- [ ] **Step 4:** Re-run the tests: PASS.

### Task 3: DoD, evidence, delivery
- [ ] Run both acceptance commands workspace-wide. Run clippy (`-p aq-agent-client -p aq-worker --all-targets -D warnings`) and `rustfmt --edition 2024 --check` on the touched files.
- [ ] Write `docs/implementation/F3.md` and the logs in `docs/implementation/F3-evidence/`.
- [ ] Squash to one commit on the source base, `git merge origin/main`, re-run the targeted tests, `aq git push`, and close.
