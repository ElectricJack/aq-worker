# H1 Runner Observations Implementation Plan

> **For agentic workers:** Execute this plan in the current held session, test-first. The task explicitly authorizes implementation without another plan approval.

**Goal:** Collect only the launched harness transcript into format5 and replay permit-authenticated batches until a matching server receipt, including after restart.

**Architecture:** A trusted harness adapter supplies an exact launched-session transcript descriptor. The runner admits it only within its authorized launch roots. A separate observation worker owns file reads and the format5 store; renewal, stop and cancellation never await that worker. Existing source identity guards, transactional normalization, bounded loss gaps and durable packets remain authoritative.

**Tech Stack:** Rust, Tokio, aq-observation =0.1.0, aq-observation-engine =0.0.1, aq-observation-redb-prototype =0.0.5.

**Spec:** AQ clear-crest-61.1 description (H1); local WRK-15-reserved-control.md, correction within unchanged hard limits; B6c runner channels.

## Global Constraints

- No second task authority; observations confer no authority or invoice truth.
- All network operations initiate from the sandbox and carry its delegated permit; no node credential.
- Real harnesses retain the existing tmux host, guardian, renewal and stopped-proof gates.
- No secrets in fixtures, logs or metadata. No raw prompt/tool content in usage facts.
- Exact version pins; 64 KiB events, 256 KiB packets, <=16 MiB spool/logical data, 64 MiB physical file, <=16 sources.
- Never scan provider homes or choose newest/cwd-similar sessions. Reject symlinks and path redirection.
- Stop proof precedes resource release. Telemetry never delays control.
- Unsupported sources and loss are explicit incompleteness; no silent cursor advancement.
- No server ledger, registration, pricing, stall ladder or terminal work (H2/H3/H7).
- Owner velocity rule: targeted named/area tests, formatting and scoped Clippy, no repeated full-suite runs. Run Cargo inside the existing AQ pytest resource-gate adapter with two build jobs/test threads.
- Evidence: docs/implementation/H1.md; publish task branch with guarded AQ push; integration owner publishes main.
- H1 has no adversarial review marker.

## Review Focus

- Restart before receipt must replay byte-identical packet and stable source event IDs.
- A foreign launch identity, path outside launch roots, symlink or inode replacement must never retarget the reader.
- Incomplete final lines must remain unconsumed; oversized records and quota overload must produce explicit bounded gaps.
- Held upload or disk work must not prevent cancellation/renewal; failed or mismatched receipt retains every event.
- Qualification must read real rollout records, preserve counters, remove personal/content fields, and assert usage includes no raw content.

### Task 1: Durable collector and qualification tests

**Files:** Create runner sandbox observations.rs and observations_tests.rs; modify aq-worker/Cargo.toml, Cargo.lock, sandbox.rs; create tests/fixtures/codex-real-0.166.0.jsonl.

**Interfaces:** Consume Store::{create,open,attach_file_engine,collect_file,prepare,acknowledge}, SourceFile and SourcePermit. Produce Collector::{open,collect,prepare,acknowledge} with persisted source identity and exact replay.

- [x] Write `observation_cursor_survives_runner_restart_without_duplicates`: collect a real sanitized rollout, prepare packet, reopen collector, assert same packet bytes/event IDs/cursor; reject wrong receipt; ACK exact receipt, reopen, collect appended suffix and assert no duplicate IDs.
- [x] Run RED (missing Collector); implement exact pins, persisted permits, authorized-root and source-binding checks; run GREEN.
- [x] Add tests for outside-root/symlink/foreign-session refusal, partial suffix and inode replacement, oversized/loss gaps and private usage; run affected area.
- [x] Record real CLI version, exact format ID, provenance and sanitization policy; commit fixtures.

### Task 2: Launch integration and delivery isolation

**Files:** Modify session-host/src/lib.rs (default trusted descriptor hook), runner sandbox.rs/channels.rs; add observation service in observations.rs and tests.

**Interfaces:** HarnessAdapter::transcript_source receives the exact LaunchIntent after launch. Observation worker consumes the descriptor plus authorized launch roots and SandboxChannels; post bounded Packet to /v1/worker/observations/batch; accept only lifecycle Receipt validated against persisted packet.

- [x] Write failing authenticated delivery/invalid receipt and held upload cancellation tests using loopback peer and synchronization, no real sleeps.
- [x] Add descriptor validation, thread-owned store and separate channel pool, stop signal and explicit diagnostics for missing/unsupported source; use GapAndHold on quota pressure. Coalesce repeated ordinary state diagnostics.
- [x] Run GREEN and runner area tests; confirm shared authority still stops a revoked runner.

### Task 3: Evidence and publication

**Files:** docs/implementation/H1.md, H1-evidence/*, this plan.

- [x] Run exact named workspace test (must report >=1 pass), affected observation/runner tests, cargo fmt --check and scoped Clippy -D warnings.
- [x] Self-review every Review Focus input; reconcile all task bullets; record commands/environment/commit/limits.
- [x] Squash unpublished implementation history to one feature commit, merge origin/main, run relevant targeted checks if main changed touched code, guarded push and explicit task close/drain acknowledgement.
