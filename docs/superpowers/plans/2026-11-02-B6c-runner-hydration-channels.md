# Runner hydration and per-sandbox channels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The sandbox runner hydrates its attempt workspace only from central retention under its permit, and holds terminal and observation traffic on per-sandbox channels that hold no poll while idle.

**Architecture:** Add `sandbox/hydrate.rs`: fetch the retention descriptor (`TransferManifest`) and its members through RunnerHttp's authorized source export, validate descriptor, headers, offsets, sizes and SHA-256 digests, stage privately with the existing node staging owner, then import into the pinned aq-git (register, import, create) and prove a clean worktree at the retained revision with retention and workspace inspection. Add `sandbox/channels.rs`: a per-sandbox channel on its own HTTP connection pool that shares the permit authority, one try-locked terminal long poll driven by a demand watch, 20 s hold and 64 KiB frame caps, and observation uploads under the same channel identity.

**Tech Stack:** Rust 2024, reqwest, Tokio, axum loopback test peer, real `git` for the fake aq-git test peer, pinned in-repository aq-admission 0.3.0 and aq-terminal-relay-protocol 0.1.0.

**Spec:** AQ task noble-summit-37.9, B6c (0005 §4 step 4, SBX16, SBX17, 0003 §5.3). Out of scope: observation readers/store (H1), renderer and full output path (H7), server routes (B2), cold remote fetch, hibernation (H5).

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


Task-specific verification follows prime's 2026-11-02 velocity rule: named and affected-area tests, formatter and targeted Clippy, without repeated full-suite runs. Execution is already authorized for this session; no extra plan approval gate.

## Review Focus

- Hydration must never take a Git remote URL or credential; the only network path is RunnerHttp to the central origin.
- Descriptor association (attempt, session, generation, spec digest, repository, base) and every chunk's transfer/offset headers must match before bytes reach disk.
- The workspace is accepted only when aq-git retention is active for the base and inspection reports HEAD == retained revision and a clean tree.
- At most one terminal poll per sandbox; no request at all while idle; hold <= 20 s; request and reply frames <= 64 KiB; terminal requests never share the renewal connection pool.

### Task 1: Runner hydration from retention

**Files:** Create `crates/aq-worker/src/native/runner/sandbox/hydrate.rs`, `hydrate_tests.rs`; modify `sandbox.rs`, `sandbox/auth.rs`, `native/http.rs`, `native/http/source.rs`, `native/preparation.rs`, `native/hydration.rs`, `native/hydration/files.rs` (visibility only).

**Interfaces:** Produce `RunnerHttp::{launch_binding, source_status, source_chunk}` (replacing the header-blind `source`), `hydrate::hydrate(&RunnerHttp, &SourceBinding, &GitClient) -> HydratedSandbox` and `hydrate::start_pinned_git(bundle_root, state_root, git) -> GitService`. Consume node `transfer`/`version`/`metadata`/`body_type` checks and `Staging`.

- [ ] **Step 1: Write the failing acceptance test.** `hydrated_workspace_matches_retained_revision`: real repo + `git bundle`, axum loopback central peer serving descriptor and members (requires permit bearer, records paths), fake aq-git socket performing a real fetch from the staged bundle and `git worktree add --detach`. Assert HEAD and tree match the retained commit, the workspace has no remote, and every request went to the source export with the permit bearer.
- [ ] **Step 2: Run RED.** Expected compile failure (missing `hydrate`).
- [ ] **Step 3: Implement.** Descriptor/header checks, chunk loop (256 KiB), staging verify/publish, aq-git ops and receipt/retention/inspection checks; bundle-pinned aq-git start.
- [ ] **Step 4: GREEN** plus a digest-mismatch refusal case (tampered bundle byte is refused before import) and existing `auth_tests` adapted to the new source API.

### Task 2: Per-sandbox terminal and observation channels

**Files:** Create `crates/aq-worker/src/native/runner/sandbox/channels.rs`, `channels_tests.rs`; modify `sandbox.rs`, `sandbox/auth.rs`.

**Interfaces:** Produce `RunnerHttp::channel_pool()` (fresh connection pool, shared authority), `RunnerHttp::post_bounded`, `SandboxChannels::{new, set_demand, poll_terminal, run_terminal, upload_observations}`, `TerminalDemand`.

- [ ] **Step 1: Write the failing test.** `idle_sandbox_holds_no_terminal_poll`: with no subscriptions/controllers the running terminal loop sends zero requests; raising demand sends exactly one permit-authenticated `RelayPoll` with `hold_ms == 20000`; a concurrent second poll is refused; an over-64 KiB reply is refused.
- [ ] **Step 2: Run RED**, then implement and run GREEN.

### Task 3: Verification, evidence and publication

**Files:** `docs/implementation/B6c.md`, logs under `docs/implementation/B6c-evidence/`, this plan.

- [ ] **Step 1:** Run both named tests and affected runner tests via `aq test` (B5a adapter, 2 jobs/threads), `cargo fmt --check`, scoped Clippy `-D warnings`.
- [ ] **Step 2:** Self-review the diff against the Review Focus (no adversarial marker on this task).
- [ ] **Step 3:** Record evidence, remaining limits (B2 server routes, production launch wiring) and follow-ups.
- [ ] **Step 4:** Squash, merge `origin/main`, targeted re-test, guarded `aq git push`, close with changes/verification/test evidence.
