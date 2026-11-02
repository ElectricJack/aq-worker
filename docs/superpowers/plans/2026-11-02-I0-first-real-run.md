# I0 First Real Run Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete three fresh Codex 0.166.0 ChatGPT runs against E0, retain their edits centrally, settle them through public routes, and preserve refreshed owner login material.

**Architecture:** Use the existing production native interactive executor in a plain Docker container with host networking and one slot. Build the worker from the current main revision using an isolated copy of aq-server's Dockerfile.worker build inputs, without re-pinning the server repository. Keep all private configuration and attempt state outside the recyclable checkout; publish only nonsecret evidence.

**Tech Stack:** Rust with --locked, Docker Desktop/WSL2, Python standard library, existing E0 PostgreSQL/Graph/Git/server.

**Spec:** Task azure-horizon-22.1 complete description; aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml I0 and program plan Global Constraints.

## Global Constraints

- Standalone locked builds; no sibling path dependencies, submodules or floating release inputs.
- aq-graph owns task state; AQ owns attempts, grants and dispositions. Use existing public routes.
- Every real CLI runs inside the existing attempt-owned tmux host under its guardian.
- Stop proof precedes release; preserve unretained source and unknown outcomes.
- No secrets in images, env, argv, logs, task descriptions or evidence. Deliver ChatGPT login only through the private material lease.
- Atomically persist any refreshed login to the owner's auth.json with mode 0600 before removing a container. Preserve state on conflict.
- No hostile-code isolation claim, admission bypass, API-key substitution, automatic settlement, retry, permits, templates or vault implementation.
- Do not install v2 under aq/agent-queue. Push only the assigned branch through AQ.
- Run formatting and focused applicable checks; unavailable checks are recorded honestly. No whole-suite CI run in this worker.
- Record both docs/implementation/I0-first-real-run.md and docs/implementation/I0.md to satisfy the explicit report and common footer.

## Review Focus

1. Rotated login: exact bytes must survive atomic private writeback; test changed bytes and resulting mode.
2. Concurrent owner refresh: refuse overwriting a changed owner file; test conflict preserves both inputs.
3. Invalid/unsafe auth path: refuse symlinks and malformed/non-ChatGPT login without printing data; test both.
4. Unchanged/missing output: report unchanged/no launch without touching owner state; test unchanged bytes and absent output.
5. Early failure: preserve the Docker container and state until login/source reconciliation; verify actual failure path and report any limits.

### Task 1: Safe qualification preparation and login writeback

**Files:** Create `scripts/i0-auth-writeback.py`, `tools/qualification/tests/test_i0_auth_writeback.py`; temporary private Docker/configuration inputs outside the checkout.

**Interfaces:** Consumes owner auth file, private initial snapshot and retained launch auth file. Produces `writeback(owner: Path, baseline: Path, refreshed: Path) -> str` returning unchanged, refreshed, or absent; exceptions reveal no credential bytes. The CLI takes file paths only.

- [ ] Write synthetic-file tests for refresh/mode, conflict, symlink/invalid-login refusal, unchanged and absent output.
- [ ] Run `aq test tools/qualification/tests/test_i0_auth_writeback.py -q`; expect RED before helper exists.
- [ ] Implement validated bounded private reads and fsync + atomic rename, preserving inputs on refusal.
- [ ] Run the same command; expect all named cases PASS. Commit.
- [ ] Build the plain worker image from copied Dockerfile.worker inputs with current main, pinned Codex, helper/client/tmux/Git. Record immutable image identity.
- [ ] Verify production check-native in Docker with actual installed pins and no test-support flags. Refusals are findings, never bypassed.

### Task 2: Three real runs and settlement evidence

**Files:** Create `docs/implementation/I0-first-real-run.md`, `docs/implementation/I0.md`; retain private requests/state under the persistent I0 state directory.

**Interfaces:** Consumes Task 1's image and writeback command, E0's designated graph task/project/repository and existing enrollment/private material/profile APIs. Produces three run records with attempt/grant/version/model/time/source/outcome and filed v1 finding IDs.

- [ ] Configure E0's existing registry and private material copy without embedding login bytes in public configuration. Use public owner/worker routes to assemble frozen attempts and source bases.
- [ ] Execute one fresh empty-state single-slot run at a time. Verify real repository edits, stopped writer, result retention and login refresh status before any cleanup.
- [ ] Use evaluation, combined review resolution and completion reconcile routes. Record actual decisions and central revision handles. Repeat until three consecutive completions; do not fabricate success.
- [ ] File each confirmed rough edge as a scoped follow-up with originating evidence; keep changes minimal.
- [ ] Run focused helper checks, image/native preflight, diff/format checks and any relevant Rust tests if Rust changes become necessary. Record exact results and limits.
- [ ] Review all changes and evidence, reconcile checklist, squash leaf implementation history while preserving the plan record, merge authorized upstream, push, close explicitly and drain-ack.
