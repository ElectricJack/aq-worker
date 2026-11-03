# B5b Controller Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking. Execute inline in the assigned AQ worktree; the task explicitly requires completing this session.

**Goal:** Recover owned sandboxes without relaunching attempts or deleting unretained work.

**Architecture:** Extend B5a's checksummed intent journal with grant monitoring, cleanup reasons, cancellations and orphan records. Reconcile backend inventory before admission; send B1 re-adoption requests through the authenticated provider seam. Independently sweep hard deadlines, retry runner extraction/central handoff, and require persisted retention/operator authorization before deletion.

**Tech Stack:** Rust, Tokio, serde, C1 PlacementBackend/FakeBackend, pinned aq-admission 0.2.0.

**Spec:** Task noble-summit-37.6 (B5b); aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml; architecture 0005 sections 4.1, 4 step 7, 6, 13.

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


The owner velocity rule in prime supersedes broad test runs: run the named filters, controller area tests, formatting and relevant clippy checks. No real backend, real sleeps or full-system tests are added. B5b is not marked adversarial.

## Files and interfaces

- `controller/types.rs`: bounded `RecoveryConfig`; serializable grant/cleanup/orphan facts; provider defaults `readopt(&ReadoptionRequest)`, `poll(&WorkerIdentity)` and `retain(&SandboxIdentity)`. Retain means attempt runner extraction of workspace AND evidence and return authorization only after central retention succeeds. Unavailable implementations preserve, never invent receipts.
- `controller/recovery.rs`: `reconcile_at(now_ms)`, `observe_grant(&DelegatedPermit)`, `observe_poll(&PollPage, now_ms)`, `sweep_at(now_ms)` and `retry_handoffs_at(now_ms)`. Time is explicit in recovery tests; production uses wall-clock deadlines on each tick.
- `controller/journal.rs`, `mod.rs`, `lifecycle.rs`, `service.rs`: durable defaults compatible with B5a snapshots, grace enforcement, resource accounting, periodic service wiring and no late acceptance revival.
- `controller/recovery_tests.rs`, `controller/recovery_ports.rs`: fake backend/controller tests for named acceptance and synthetic recovery equivalents. Existing tests cover unchanged capacity, ownership and malformed-state behavior.
- `backend/fake.rs`: deterministic operation/failure evidence as necessary.
- `Cargo.toml`, `lib.rs`, `main.rs`, `guardian.rs`, `source.rs`, `source_transfer/*`: gate synthetic execution/journal/driver behind explicit `synthetic-runtime` test feature; production binary omits `run-execution`.
- `docs/implementation/B5b.md`: exact commands, results, baseline/final receipt, port mapping and limits.

### Task 1: Durable recovery and re-adoption

- [x] Add failing fake-backend tests for restart inventory, lost create response, re-adoption request identity/generation, expired renewal and no relaunch. Preserve the backend instance while dropping/reopening the controller.

```rust
let backend = std::mem::take(&mut c.backend);
let boot = c.identity().boot_id;
drop(c);
let mut c = reopen(root, backend);
c.register().await?;
c.reconcile_at(now).await?;
assert_ne!(boot, c.identity().boot_id);
assert!(c.launch(attempt, &()).await.is_err());
```

- [x] Run `cargo test --locked -p aq-worker --lib controller::recovery_tests` through the existing AQ Cargo adapter; capture failing assertions or unavailable symbols.
- [x] Add durable optional fields using serde defaults and omission for empty fields so format-1 checksums remain compatible. Record recovery requests before sending; reuse an operation ID on retry. Group previous boots and chunk B1 requests at 128 sandboxes.
- [x] Validate inventory full identities/template/deadline before adopting mappings. Hold missing/contradictory entries. Re-adopt current execution grants without create/start; allow runner renewal loss to stop pending re-adoptions. Stop unknown or ungranted entries, journaling cleanup reason before effects.
- [x] Run the recovery tests and existing controller tests, then commit.

### Task 2: Preservation, sweeps and cancellation

- [x] Add `unretained_sandbox_is_preserved_not_destroyed`, grace/retry/restart, stop-failure, foreign-scope, poll replay, and hard-deadline tests.

```rust
c.provider.retention_available = false;
c.sweep_at(deadline).await?;
c.retry_handoffs_at(deadline).await?;
assert!(c.backend.inspect(&sandbox).await?.disk_present);
c.provider.retention_available = true;
c.retry_handoffs_at(deadline + grace).await?;
assert_eq!(c.backend.inspect(&sandbox).await?.state, SandboxState::Destroyed);
```

- [x] Capture the red run using the same focused filter.
- [x] Persist cleanup reasons/deadlines and authorization. Retry extraction/handoff; on failure stop and preserve disk. Release compute only on backend proof and disk only on authorized confirmed destruction. Unknown orphan resource usage fences admission until resolved.
- [x] Set backend deadline to grant hard deadline plus bounded margin. Sweeps only stop, including unknown owned inventory; handoff/deletion is a separate phase. Poll cancellation deadlines are first-observation plus bounded grace; duplicates cannot postpone them. All admission/backend launch effects reject pending cancellation or expired authority.
- [x] Run a service tick that polls, reconciles/re-adopts, sweeps and retries handoff. Provider errors do not suppress local deadline enforcement. Shutdown preserves state.
- [x] Run focused tests and commit.

### Task 3: Synthetic recovery ports and production retirement

- [x] Port controller-relevant assertions from all 17 execution tests, mapping runner/transport-only assertions to the task boundary explicitly. Add named equivalents for lost acceptance, ambiguous launch intent, pending storage bounds, cancellation stop proof and stopped handoff replay before new registration. Use fake backend process counts, persisted operation IDs and deterministic failures, without real processes or sleeps.
- [x] Fix any exposed controller defects; make reservation replay distinguish identical from changed bytes; retain unresolved disk/result capacity and fail closed on journal errors.
- [x] Gate runtime/journal/driver and `run-execution` under `synthetic-runtime`, enabled explicitly for legacy test harnesses. Keep shared guardian/source types available to native execution. Verify a normal production `cargo check --locked -p aq-worker --bin aq-worker` without the feature.
- [x] Run controller tests, both exact named workspace filters, relevant backend/CLI tests, `cargo fmt --check`, and `cargo clippy --locked -p aq-worker --all-targets -- -D warnings`. Record the 17-test mapping and harness-only decision.
- [x] Commit evidence, squash only this leaf's implementation commits onto recorded base `9358faf1958fc1950c7266f5be2ad7820cdef82c`, sync/merge origin/main, rerun targeted checks if source changed, guarded push, settle subtasks and close with checks.

## Execution notes

Completed inline with FakeBackend; evidence and the 17-test port ledger are in
`docs/implementation/B5b.md`. The initial red run used the broader controller
filter and captured missing recovery APIs, not behavioral assertion failures.
The implementation additionally requires final stopped-writer proof before
retention, preventing writes after a successful receipt. Source and evidence
commits are consolidated at completion under the AQ leaf-squash policy rather
than preserving the per-section commit suggestions above. Production Clippy and
a default-feature CLI run supersede the planned production `cargo check`.
Publication and final sync OIDs are recorded in the AQ task receipt.
