# B6a1 Sandbox Runner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Execute one frozen local launch under the existing guardian/tmux host, renew bounded authority, preserve on loss, and report completion only after central retention receipts.

**Architecture:** Add a separate fresh-process handoff to `native::runner`, backed by a one-shot private state directory and the reusable `native::launch`/`aq-session-host` host. Keep the existing native journal recovery fence intact. One `RunnerTransport` trait owns authenticated renewal, retention and completion; the supervisor authorized a fixture implementation and explicitly deferred production HTTP wiring to B2/B6a2 in message msg-d369fb8d69a14dffa0c924bab36180da. Production CLI refuses unavailable transport before launch, without inventing a route.

**Tech Stack:** Rust 2024, Tokio, aq-admission 0.2.0, aq-session-host, Linux CLOCK_BOOTTIME, tmux 3.4, deterministic aq-fixture-agent.

**Spec:** AQ task noble-summit-37.11 (B6a1), plus the supervisor boundary above; parent task contract is aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml, B6a1.

## Global Constraints

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Contracts:** a wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version. The task is not done until valid, invalid, replay and compatibility cases pass on both sides (ecosystem §3).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Hosting:** every real agent CLI runs in an attempt-owned tmux host under the guardian (H1), inside its own sandbox (SBX1). Central supervisors run in the supervisor host (DEP5).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Isolation claims:** do not claim hostile-code isolation for shared-kernel containers (W6, SBX10).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- **Naming:** while v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- This task's explicit interim exception uses existing worker configuration for transport authentication; the harness never receives it. Permit credentials stay outside the frozen manifest/journal.
- No hydration, observation/terminal transport, sandbox backend, OS-user setup or Codex qualification work. Local prepared inputs enter through the library host adapter.
- Required checks: cargo fmt --check; cargo clippy --locked --all-targets -- -D warnings; cargo test --locked, including both named acceptance filters. Evidence: docs/implementation/B6a1.md. AQ test has no Cargo backend; bound Cargo jobs/test threads locally.
- No adversarial review marker exists for B6a1. Execute inline with one fresh final code review per executing-plans; integration remains AQ-owned.

## Review Focus

- A replay or concurrent invocation must not start a second writer, including after failed renewal or incomplete handoff.
- A stalled renewal must not prevent shutdown/expiry; suspend advances CLOCK_BOOTTIME and cannot extend authority.
- A late, mismatched or replayed renewal must not extend a stopped or expired guardian; the original hard deadline remains a cap.
- Pane exit, EOF and tmux acknowledgement alone cannot authorize retention/reporting; unknown stop preserves everything.
- A missing, mismatched or failed retention receipt must leave completion unpublished and original evidence intact.

---

### Task 1: One-shot handoff, guarded runner and typed transport

**Files:**
- Create: `crates/aq-worker/src/native/runner/sandbox.rs`, `sandbox/manifest.rs`, `sandbox/transport.rs`, `sandbox/journal.rs`.
- Create: `crates/aq-worker/src/native/launch/sandbox.rs`.
- Modify: `crates/aq-worker/src/native/runner.rs`, `native/launch/mod.rs`, `main.rs`, `bin/aq-fixture-agent.rs`, `crates/aq-worker/Cargo.toml`.
- Test: `crates/aq-worker/tests/sandbox_runner.rs` and `tests/support/sandbox_runner.rs`.

**Interfaces:**
- Consumes: `SessionHost::launch`, `SessionHandle::{renew,stop,wait_stopped,stopped_proof}`, `HarnessAdapter`, `LocalEnvironment`, B1 `PhaseExecutionAuthority`, `RunnerRenewalRequest/Response`, `WorkerConfig`.
- Produces: `FrozenLaunch::load(path)`, `sandbox::run(worker, launch, permit, state_dir, local, adapter, transport, shutdown) -> Result<RunOutcome>`, and one `RunnerTransport` trait. Retention and completion use typed local observations; they are not new HTTP schemas.
- `FrozenLaunch` binds the manifest digest, phase origin, exact state directory and kernel boot, launch ID, observed grant/hard deadlines. The credential is a separate private input. A consumed marker is durable before any writer; ordinary native journals never regain launch authority.
- Renewal runs every 15 seconds by default (120-second maximum individual local grant), also while waiting for transport responses. Refusal stops immediately, transient failure/re-adoption never extends, and expiry/shutdown races cancel only the transport wait. Physical stop always runs. Validated renewal is journaled before guardian installation. Response validation stays in the transport seam for the pending B1 correction.
- Completion requires physical proof, retained source receipt and retained result-evidence receipt, all bound to the attempt. Persist receipts before invoking `finished`; persist its acknowledgement before returning Finished. All other outcomes preserve state and cannot relaunch.

- [ ] **Step 1: Write failing CLI and lifecycle tests.** `sandbox_run_cli_requires_frozen_manifest_and_state` checks the CLI registration; `renewal_loss_stops_and_preserves_without_relaunch` launches the real fixture with an escaped descendant and checks stopped proof, stable workspace bytes and refused second invocation. `runner_cannot_report_before_handoff_receipts` withholds each receipt in turn and requires no completion; a full handoff records completion after both receipts.
- [ ] **Step 2: Run tests and observe missing runner behavior.** `cargo test --locked -p aq-worker --test sandbox_runner`; expected failure before implementation.
- [ ] **Step 3: Implement the interfaces above.** Use private fsync-backed one-shot journal files, no release/delete path, bounded validation and the existing guardian host. Add fixture helper dispatch to aq-fixture-agent for the same host library, without invoking real providers. CLI `sandbox-run --manifest PATH --state-dir PATH` parses trusted frozen input and explicitly refuses the unavailable production transport before mutation.
- [ ] **Step 4: Cover the Review Focus conditions.** Add concurrent/replay, stalled renewal, renewal success and hard cap, invalid receipt, unknown stop, cancellation, and mismatched/expired handoff cases. Use real host processes and deterministic transport only; assertions inspect persisted state, physical process proof and retained artifacts.
- [ ] **Step 5: Run focused checks and commit.** `cargo test --locked -p aq-worker --test sandbox_runner` must report every new test passing. Commit implementation and its tests together.

### Task 2: Verification, evidence and publication

**Files:** `docs/implementation/B6a1.md`, this plan.

**Interfaces:** consumes Task 1's runner, tests and journal evidence; produces reproducible qualification evidence and a pushed leaf commit.

- [ ] **Step 1: Run both exact named acceptance filters.** `cargo test --locked --workspace runner_cannot_report_before_handoff_receipts` and `cargo test --locked --workspace renewal_loss_stops_and_preserves_without_relaunch`; require one actual passing named test each.
- [ ] **Step 2: Run definition-of-done checks.** Run formatter, Clippy and Cargo tests as specified above; record failures by name, never waive them silently.
- [ ] **Step 3: Write evidence.** Record environment, source/implementation revisions, exact checks, one-shot/stop/retention behavior and the explicit lack of server compatibility proof or production HTTP transport. Record the B1 API revision status.
- [ ] **Step 4: Obtain one fresh whole-branch code review and resolve material findings with regression tests.** No cross-family adversarial AQ gate is required by this task's marker.
- [ ] **Step 5: Publish.** Reconcile the recorded source base, squash unpublished implementation history while preserving merge ancestry, update from the configured integration branch, run targeted checks for any merge changes, guarded aq git push, and close with changes/verification/test evidence. If origin/dev is absent, request the supervisor's repository-specific equivalent instead of guessing.
