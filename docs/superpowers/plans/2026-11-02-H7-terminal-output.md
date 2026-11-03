# H7 Terminal Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Publish complete renderer snapshots and sequenced output over the runner's independent terminal lane, with bounded replay and reconnect barriers.

**Architecture:** A single local sequencer feeds the strict renderer before replay eviction. A snapshot captures its exact epoch and output watermark under that sequencer's lock; upload retains immutable pending exchanges and sends only subsequent frames. Only a verified pre-output blank baseline admits a complete renderer. Permit-authenticated sandbox output has its own bounded queue and connection pool, independent of renewal and cancellation.

**Tech Stack:** Rust, aq-terminal-renderer 0.1.0, existing Snapshot/OutputFrame contracts, Tokio, tmux control observer.

**Spec:** Task clear-crest-61.7 (H7), supplied task description; 0003 §5.3 and R4–R5.

## Global Constraints

- Each repository builds and tests alone; no sibling path dependencies; committed lockfile with `--locked`.
- Existing opaque `Snapshot.state` carries AQRI bytes without a wire change.
- All connections originate from workers/runners/sandboxes; execution authority remains with AQ.
- Never manufacture a blank physical baseline, output gap extent or new epoch after observer loss.
- No secrets in manifests, logs, evidence or task descriptions.
- Frames ≤64 KiB; session output ≤256 KiB/s; node output ≤2 MiB/s; flush target 50 ms.
- Session replay ≤1 MiB; node replay ≤32 MiB; ≤4 viewers/session and ≤32 subscriptions/node.
- One terminal poll/sandbox, hold ≤20 s, no idle poll. Terminal work uses independent bounded queues/pools.
- Owner velocity rule: targeted named/area tests; no full-suite qualification or slow tests.
- Evidence: `docs/implementation/H7.md`; commit and guarded publication before close.

## Review Focus

- Output buffered while a snapshot upload is stalled must be applied exactly once after its watermark.
- Unsupported terminal sequences and unknown observer discontinuity must invalidate completeness.
- Resize/layout changes cannot silently preserve an incorrect complete image.
- Slow/failed uploads and replay eviction must preserve exact gaps and immutable retry bytes.
- Flooding cannot acquire renewal/cancellation locks or unbounded queue capacity.

### Task 1: Sequenced renderer and snapshot cuts

**Files:** Create `crates/aq-worker/src/native/terminal_output.rs` and tests; modify worker Cargo manifest/lockfile and `native/mod.rs` module declaration as appropriate; integrate `native/launch/output_relay.rs`, `native/launch/native.rs` and `native/launch/mod.rs`.

**Interfaces:** Consumes `VerifiedBlankBaseline`, `ControlEvent`, `Renderer::{bytes,resize,gap,image,restore}` and existing `OutputTail`. Produces a bounded rendered output owner whose snapshot is `Snapshot` and whose replay frames/gaps retain existing literal-byte semantics.

- [x] Write `reconnect_after_gap_renders_from_snapshot`: force replay eviction, capture/serialize/restore a snapshot, buffer split UTF-8/ANSI on both sides of the cut, and assert literal expected cells after following frames with no duplicate application.
- [x] Run `cargo test --locked --workspace reconnect_after_gap_renders_from_snapshot`; expect failure because the new snapshot owner is absent.
- [x] Implement ordered byte/resize ingestion, epoch reset, invalidation and snapshot export with identity/watermark validation. Use the existing opaque image contract; refuse unsupported state rather than inventing it.
- [x] Add focused invalidation, retransmit/conflict, epoch and resize tests. Integrate the verified native baseline and retain all bootstrap bytes. Keep raw control output as the renderer source.
- [x] Run named test and terminal output area tests; expect nonzero passing tests. Commit.

### Task 2: Sandbox output queue and isolation

**Files:** Modify `native/runner/sandbox/channels.rs`; create `native/runner/sandbox/output.rs` and tests; modify runner/host startup integration as resolved with the supervisor, and `native/runner/publish_output.rs` flush interval.

**Interfaces:** Consumes Task 1's renderer snapshots/replay, B6c `SandboxChannels` and permit-scoped `RunnerHttp`. Produces one bounded independent output upload lane, verified snapshot reconnect and rate-limited terminal traffic.

- [x] Write `output_flood_does_not_delay_renewal`: hold the actual output HTTP request, flood the bounded producer, complete an independent permit-authenticated renewal request, and signal cancellation while upload remains blocked. Assert queue bounds, explicit eviction gaps and exact retry body.
- [x] Run `cargo test --locked --workspace output_flood_does_not_delay_renewal`; expect failure because output upload is absent.
- [x] Implement one bounded pending upload plus bounded replay; independent connection pool; session rate budget and node aggregate enforcement; 50 ms flush target. Reuse B6c's demand/poll cursor and enforce viewer limits.
- [x] Wire only verified startup sources into output. Generic post-start capture cannot qualify a blank baseline; resolve the missing sandbox observed-startup join with the supervisor before claiming that path complete.
- [x] Run the named tests and related channel/output area tests. Run formatting and targeted strict Clippy. Write `docs/implementation/H7.md` with exact commands, environment, revision and limitations.
- [x] Review the final change, settle task checklist, squash own implementation history, merge origin/main, rerun impacted targeted checks, push via `aq git push`, and close with evidence.
