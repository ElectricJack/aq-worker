# Runner permit authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every sandbox runner request uses its private delegated permit and refuses foreign, expired, wrong-boot or premature execution authority before sending.

**Architecture:** Add a dedicated RunnerHttp boundary with closed route selection, B1 scope validation, boot-clock deadlines and shared authority-loss state. Adapt B6a1 through PermitTransport<E>, with an injected RunnerExchange for unpublished B2 retention/completion envelopes. Existing node clients remain node-only; sandbox callers use RunnerHttp without WorkerConfig credentials.

**Tech Stack:** Rust 2024, reqwest, Tokio, pinned in-repository aq-admission 0.3.0, Linux CLOCK_BOOTTIME.

**Spec:** AQ task noble-summit-37.12, B6a2; supervisor message msg-0730a80b65a14d709d14e053bb9142b1 authorizes fixture-backed transport envelopes pending B2. No final wire schema or production server compatibility is claimed.

## Global Constraints


Every task's requirements include these.

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Contracts:** a wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version. The task is not done until valid, invalid, replay and compatibility cases pass on both sides (ecosystem §3).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Hosting:** every real agent CLI runs in an attempt-owned tmux host under the guardian (H1), inside its own sandbox (SBX1). Central supervisors run in the supervisor host (DEP5).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Isolation claims:** do not claim hostile-code isolation for shared-kernel containers (W6, SBX10).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- **Definition of done:**
  - `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` pass for the touched Rust workspaces. For aq-dashboard: `pnpm lint`, `pnpm typecheck` and `pnpm test`.
  - Evidence goes in `docs/implementation/<TASK-KEY>.md` in the owning repository, recording exact commands, environment, commit and remaining limits.
  - Changes are pushed through the project's integration policy.
  - Prior qualification statements are not reused for changed code.
- **Naming:** while v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- **Review:** tasks marked `review: adversarial` require a cross-family adversarial review (`aq-reviews` recipe) before a passing close. They integrate by pull request; all other tasks integrate `direct`.
- **Named tests are the evidence:** each test deliverable names a specific test. It is met only when that test exists and its run reports it passed. A filter that matches zero tests is an unmet deliverable. The owner or supervisor reviews any `pass` close that declares `--deliverable-unmet`.
- **Docs steward:** only A-series and docs tasks edit `aq-server/docs/architecture/*` or any repository's `docs/specs/*`. Other tasks record design-document changes in their evidence and file a `docs` task, so parallel worktrees don't conflict on the canonical specs.


Task-specific verification follows prime's 2026-11-02 velocity rule: named and affected-area tests, formatter and targeted Clippy, without repeated full-suite runs. The supervisor permits the B2 contract integration limit above. Execution is already authorized for this session; no extra plan approval gate.

## Review Focus

- A foreign attempt or boot nested in a request must never reach the socket.
- A request queued behind another operation must recheck deadline immediately before send.
- Awaiting node re-adoption must preserve the original local deadline, including repeated replies.
- A refusal on any channel must stop the running guardian and preserve evidence, even during renewal.
- A permit upgrade or renewal must preserve exact permit/channel identity and cannot revive expired authority.

### Task 1: Permit-only HTTP boundary and runner integration

**Files:** Create `crates/aq-worker/src/native/runner/sandbox/auth.rs`, `auth_tests.rs`, `permit_transport.rs`; modify `sandbox.rs`, `sandbox/transport.rs`; create `crates/aq-worker/tests/runner_permit.rs` and use existing sandbox support fixtures.

**Interfaces:** Consume B1 `DelegatedPermit::validate_for`, `PreparationAuthority`, `PhaseExecutionAuthority`, `RunnerRenewalResponse::validate_for` and `client_deadline_ns`, and B6b `RunnerFiles::read_secret`. Produce `RunnerHttp::from_file`, typed `RunnerRoute`, `RunnerHttp::post/get_source`, validated grant/renewal installation, `PermitTransport<E: RunnerExchange>` implementing B6a1 `RunnerTransport`. RunnerExchange accepts RunnerHttp and local typed renewal/retention/finished inputs, never node credentials. RunnerTransport exposes default `authority_lost() -> bool` for stop-and-preserve on shared channel refusal.

- [ ] **Step 1: Write the failing acceptance test.** `runner_authenticates_with_delegated_permit_and_refuses_foreign_scope` uses B1 published fixtures and a loopback recording peer: every listed route gets the permit bearer and no node/Access headers; foreign attempt/boot, expiry and execution-before-grant produce zero requests. Include private-file construction and nested foreign request fields.
- [ ] **Step 2: Run RED.** `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 cargo test --locked --workspace runner_authenticates_with_delegated_permit_and_refuses_foreign_scope`; expected missing RunnerHttp implementation.
- [ ] **Step 3: Implement the boundary.** Closed route/method mapping; bounded JSON requests and response bytes; disabled redirects/retries/proxies; sensitive bearer header; B1 validation plus stricter preparation routes from task. Map server refusals to shared authority loss. CLOCK_BOOTTIME anchored to original delivery and kernel boot must be checked on every send and after response.
- [ ] **Step 4: Add RED/GREEN runner and renewal tests.** `runner_needs_no_node_credential`, `runner_channel_refusal_stops_and_preserves`, `runner_readoption_replies_extend_nothing`, `runner_grant_upgrade_and_renewal_keep_original_scope` exercise the production boundary through fixture exchanges. Reuse B6a1 guardian tests only for stop proof; no new real sleeps, slow integration or real-network cases.
- [ ] **Step 5: Implement PermitTransport and runner loss hook.** Remove sandbox-run's WorkerConfig credential requirement, preserve node identity check, use shared authority loss during the existing guardian loop. Pending-B2 exchange owns receipt codecs; never present local DTOs as final wire schemas or enable production launch with invented endpoints.
- [ ] **Step 6: Verify GREEN and commit.** Run the named test and affected runner tests; commit code/tests and update ledger.

### Task 2: Verification, review, evidence and publication

**Files:** `docs/implementation/B6a2.md`, this plan, test logs under `docs/implementation/B6a2-evidence/`.

**Interfaces:** Consumes Task 1 APIs and tests. Produces exact commands/results, environment, revision and pending-B2 assumptions, plus published task branch.

- [ ] **Step 1: Run focused checks.** Exact acceptance command must report a nonzero test count. Run affected runner tests, formatter, and Clippy with `--locked --all-targets -- -D warnings` scoped to aq-worker.
- [ ] **Step 2: Review.** Self-review the complete diff against scope and all five review conditions; no adversarial marker is present. If executing-plans requires a fresh final reviewer, dispatch one reviewer for this branch and resolve material findings with regression tests.
- [ ] **Step 3: Record evidence.** Document fixture-only envelope fields and supervisor authorization; CLI remains explicit about unavailable B2 production codec. Preserve exact failures and commands.
- [ ] **Step 4: Publish and close.** Squash unpublished leaf changes while preserving tree/attribution; merge origin/main into assigned branch, run targeted checks for changed files, guarded aq git push, record SHA, close explicitly with changes/verification/test evidence, then drain-ack.
