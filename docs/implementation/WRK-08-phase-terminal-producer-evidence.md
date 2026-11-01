# Paired phase terminal producer qualification

Source `66aa5c8c5839e440c401c11705f48aad6fca873e` adds the
[test-support-only producer](WRK-08-phase-terminal-producer-plan.md), exact archived
phase-terminal0.1.0 dependency, and explicit journal format10. No host core/wire,
provider, default driver, renewal, resource settlement or main branch is changed.
No no-Started closure publication or complete phase lifecycle advertisement is
qualified. Missing admission/ACK after expiry stays held.

The exact public archive SHA256 is
`1b3e4afbd9356f6d7027b0a4be3fe37e2231b55bdfa10269c87a84bf9568f0bb`.
All18 extracted members remain unchanged. The14 admission019, one session0.1.0
and four lifecycle0.1.0 source members match their existing frozen archives. Only
consumer workspace dependency patches are added; no public manifest is rewritten.

Qualification on shared x86_64 WSL2, Rust/Cargo1.96.1, offline, debug0,
incremental0 and two Cargo build jobs:

- Package:227 pass/21 opt-in ignores with `RUST_TEST_THREADS=2`.
- Actual tmux3.4 host:15 top-level cases pass95.73s, one test thread. The isolated
  short-exec child separately passes3.78s and is not a sixteenth top-level case.
- Actual pinned Git:3 pass16.79s, separately from the paired tmux/synthetic-peer
  tests. This does not establish a joined daemon+Git+tmux terminal route.
- Strict all-target/all-feature worker Clippy passes8.27s; no-default-features
  check passes4.05s; formatting passes. Final budget tests2 pass.

The actual host matrix retains prior positive FDexec, blank baseline, independent
expiry/stop while the writer is paused, guardian loss/Unknown, root replacement,
short exec/no-Started, stop-only restart, cancellation and twelve paired launch
write cuts. New cases check genuine Running→Stopped with exact phase origins,
503 lost ACKs, wrong ACKs, exact request retries, immutable cached readback,
restart-only history and four ACK cuts (before write/after rename for each kind).
A canceled waiter after the ACK writer owns fsync does not cancel its commit or
release resources. Changed otherwise-valid ACK bytes cannot replace the first.

Four loopback transport cases additionally check exact headers/direct receipts,
no implicit retry/fallback, wrong/oversized/versioned responses, and Stopped
progress while both Started slots are blocked. These peers are synthetic and
prove neither SQL authorization nor kernel attestation. Local physical facts
come from the separately pinned actual synthetic-host helper and tmux.

The first default-parallel package run is retained:102 passed, one failed and18
ignored before later targets. The unchanged existing100ms initial-grant presence
fixture expired during setup, before its intended negative read; its unchanged
focused rerun passed. No test/runtime TTL or retry was widened. The final
reservation check avoids adding whole-state encoding to paths with no host
history, and the final declared package profile is two threads. There is no
claim that the final default-parallel full suite passed. A superseded strict-lint
failure on a constant test assertion is also retained; it became a const block,
with no runtime change.

The package run used final product code. After it, only ignored ACK-test coverage
and the constant test-assertion style changed. The final actual matrix, targeted
budget rerun and strict all-target lint cover those changes; no additional full
normal run is claimed for test-only edits. Source hashes identify exact frozen
bytes. Timings describe these finite test processes, not an admission or scaling
envelope; quiet windows were coordinated on a shared host.

The logical1MiB-per-host reservation covers512KiB of bounded private history,
four64KiB public documents and1024bytes of container/counters, below1MiB. The
fixed synthetic generator is tighter: intent/reference/path fields are bounded,
Running body and duplicate observation each fit the frozen16KiB body, matching
Stopped observation fits its16KiB body, and baseline/guardian are fixed typed
identities. The existing8MiB snapshot limit is unchanged. All writes/reopen
recheck remaining headroom, and20-digit global revision accounting prevents
counter growth spending it. Formats1–9 refuse unchanged; no reset, deletion or
automatic migration is offered. Physical ENOSPC/fsync failure remains uncertain
and held, despite logical reservation.

[Raw evidence](evidence/wrk08-phase-terminal-producer-66aa5c8/report.json) contains
1299 source hashes, dependency equality, actual local binary pins, commands,
positive and negative logs. This remains an isolated source checkpoint awaiting
root review/integration and independently qualified daemon consumers.
