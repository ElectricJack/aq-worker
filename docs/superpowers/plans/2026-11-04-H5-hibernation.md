# H5 Hibernation Checkpoint and Continuation Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A waiting agent hibernates: the runner checkpoints its work to central retention, reports a distinct suspended disposition and ends the attempt; a later continuation attempt hydrates that checkpoint in a fresh sandbox and gates normal operation on an exact `BootstrapAck`.

**Architecture:**
- New module `crates/aq-worker/src/hibernation/` consuming the CON-03 types from `aq-lifecycle-protocol` (new dependency).
  - `journal.rs`: one durable JSON record per operation (`guardian::durable_replace`), bound to the digest of the first-received payload. A repeat with the same digest recovers the same transition; a different digest is refused.
  - `credentials.rs`: the sandbox credential layout (`permit.json`, `provider.key`, `access.json`, `agent-home/claude/.credentials.json`, `agent-home/codex/auth.json`). `scrub` deletes each file and keeps only a SHA-256 fingerprint plus zeroized in-memory bytes for scanning. `scan` refuses retained entries at credential paths and any retained bytes containing a scrubbed secret.
  - `suspend.rs`: `Suspender` runs §3 steps 1–6 over a `SuspensionPorts` trait. Each step is journaled before its next effect, and the next phase is recorded after the effect. A `Boundary` hook fires after every durable write, so tests can crash at every journal/stop/snapshot/upload/receipt/release boundary and then recover from the same directory.
  - `continuation.rs`: `ContinuationBootstrap` (§4) validates the attachment against the local fresh grant, fetches each retained receipt from central retention (no local cache needed), verifies its digest and length, restores the source and summary into the new sandbox, and starts a fresh session with fresh credentials. It keeps normal operations closed until AQ returns a `BootstrapAcknowledgmentRecorded` receipt for an ack whose message id is journaled once and reused on retry.
- Ports are traits so H4 (routes), B6c (hydration), B5b (preserve/re-adopt) and the aq-git dirty snapshot can plug in production adapters. Tests use fakes, including `ConformancePeer` as the AQ endpoint.

**Tech Stack:** Rust 2024, serde_json, sha2, zeroize, tempfile (tests), `aq-lifecycle-protocol` 0.1.0.

## Global Constraints
- Summary is at most 16,384 bytes (`SummaryText`), and the intent's `max_summary_bytes` is enforced. A summary is a claim, never stop proof.
- No snapshot, retention or compute release without `StopObservation::ReportedStopped`. An unknown stop is an explicit `Held` disposition.
- Parking disposition is never success or failure; `ParkingReport::validate()` must pass.
- Compute release never implies protected-disk release; disk release needs an explicit directive.
- Credentials are deleted before snapshot and retention. The scan runs before every upload; a hit holds the transition.
- Targeted tests only; no real sleeps or network.

## Review Focus
- Crash recovery: no repeated stop after a durable stop record, the upload resumes from the journaled offset with the same artifact/operation id, and the parking report reuses the same message id.
- Lost ack keeps the same message id; a stale predecessor ack never opens the gate.

---

### Task 1: Journal + credentials
**Files:** create `crates/aq-worker/src/hibernation/{mod,journal,credentials}.rs`; modify `crates/aq-worker/src/lib.rs`, `crates/aq-worker/Cargo.toml`.
- [ ] Unit tests: journal refuses a different intent digest; scrub removes every known file; scan flags credential paths and embedded secret bytes.
- [ ] Implement; `cargo test -p aq-worker --lib hibernation`.

### Task 2: Suspender
**Files:** create `crates/aq-worker/src/hibernation/suspend.rs`, `crates/aq-worker/tests/hibernation.rs`.
- [ ] Test `hibernation_leaves_no_credentials_in_retained_state`: plant credentials in the sandbox and in the workspace, hibernate, and assert that the files are gone and that no retained artifact holds credential paths or secret bytes. Also assert that a planted leak holds the transition.
- [ ] Test `suspension_recovers_at_every_boundary`: crash at each boundary, recover, and get an identical report with no duplicate effects.
- [ ] Tests: unknown stop → Held, no upload, no release; disk release only with a directive.

### Task 3: Continuation bootstrap
**Files:** create `crates/aq-worker/src/hibernation/continuation.rs`; extend `tests/hibernation.rs`.
- [ ] Test `continuation_hydrates_checkpoint_in_new_sandbox`: hibernate on node A, then wake in a new sandbox dir on node B from central retention only. The source (including dirty/untracked files) and the summary are restored, the fresh session gets fresh credentials, and operations open only after the exact ack.
- [ ] Tests: lost ack retry keeps the message id; stale predecessor/generation ack refused; missing artifact/digest mismatch refuses wake.

### Task 4: Evidence
- [ ] `docs/implementation/H5.md` with commands, results, commit, limits and follow-ups.
