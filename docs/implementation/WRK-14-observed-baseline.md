# WRK-14 observed baseline and sealed-exec component checkpoint

Source: `6cd0c1516a6b7b5dec0616640dfd3e954c7f73ef`, isolated
`task/wrk14-observed-baseline`, based on worker `7d2dcd0`. Design correction:
`67e53bd`. This is an additive host **0.1.4 candidate**, with no worker/native
runtime adoption, main merge, package publication or deployment. Existing public
HostObservation, GuardianIdentity, LaunchIntent and StoppedProof wire shapes are
unchanged; legacy/rendered launches omit the new private optional field.

## Qualified slice

The observed path holds the exact owned pane in Starting, with the fixed helper
closed before native exec. Guardian-owned program admission transfers an actual
sealed memfd over bounded owner-only local packets before pane creation. The
explicit process-local pool charges at most4MiB/image, four entries/16MiB, with
64KiB copy/hash scratch and no eviction. The embedding still owns aggregate
admission across pools/old owners; memory/CPU/process hard ceilings are not
implemented. Executable memfd and immutable seals are required, with no fallback.

The narrow blank baseline requires the exact physical helper and terminal,
TIOCGWINSZ80x24 in the actual fixture, fresh tmux3.4/status-off pane, one-shot
read-only ignore-size observer, attached/drained client, no pane bytes before the
baseline, and guardian-side acknowledged original-time EAGAIN drain plus live,
non-SIGSTOPed reader. Geometry/helper identity are rechecked before release.
`VerifiedBlankBaseline` is opaque and Serialize-only, not a Snapshot or a resize
barrier. It identifies the exact installed renderer reference and immutable
configuration; its constructor is not available to callers or JSON decoding.

The caller's `ObservedLaunchAuthority::record_baseline` must durably accept the
exact baseline before a release packet can be sent. The guardian records one
native-exec intent and rechecks its directory, current authority, geometry and
reader after persistence. The helper revalidates seals and literal final startup
payload and uses execveat against that same FD. Gate readiness never extends the
original permit, wall limit or supplied original hard deadline.

Running/record_started require CLOEXEC peer closure **and** a still-live original
PID/startticks whose `/proc/PID/exe` device/inode match the sealed image. EOF alone
is insufficient. Short execution that ends before positive observation remains
post-grant uncertainty with no Started, even though a durable exec intent exists.
Neither that ambiguity nor a failed durability callback implies NeverGranted or
permission to relaunch/release resources. `poll_started` is nonblocking so the
caller can keep draining output.

## Checks against frozen source

Retained files are in [the evidence directory](../evidence/wrk14-observed-baseline/).
`source-inventory.json` hashes839 source/document/fixture files excluding retained
evidence; `changed-source.json` hashes all16 implementation-commit paths. The
artifact manifest hashes every retained evidence file except itself.

| Check | Result |
|---|---|
| Full workspace tests, all features, locked/offline, four test threads | 351 passed,43 ignored,0 failed across84 emitted result lines |
| Full actual tmux host suite | 36 top-level tests passed,0 failed;4.97s test process |
| Actual tmux/HTTP bridge suite, all features, four test threads | 3 passed,0 failed;1.11s test process |
| Full workspace strict all-target/all-feature Clippy | Passed |
| Full workspace formatting check | Passed |

The workspace Cargo build reported34.09s; the sum of emitted test-process
durations was174.24s. Neither is an end-to-end wall measurement. The host test
process uses default libtest parallelism (parent RUST_TEST_THREADS unset); the
missed-exec case explicitly invokes one isolated child test to prevent process
environment fault injection affecting other tests. The normal workspace leaves
all43 explicit opt-in tests ignored; the36 host and3 bridge cases above were run
separately with `--ignored`. No services/providers/network accounts were used;
bridge HTTP peers are synthetic local listeners.

Commands, run from the isolated worktree with CARGO_INCREMENTAL=0:

```text
CARGO_TARGET_DIR=/tmp/wrk14-baseline-target cargo test -p aq-session-host --features test-support --test local_tmux --locked --offline -- --ignored --nocapture
CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target RUST_TEST_THREADS=4 cargo test --workspace --all-features --locked --offline
CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target RUST_TEST_THREADS=4 cargo test -p aq-terminal-bridge --all-features --test local_relay --locked --offline -- --ignored --nocapture
```

The two targets contain normal source only; no modified fault-variant source was
built into either target. The short-exec fault is an explicit test-support hook
in the same frozen source. Qualification runs the existing guardian/reader
crash, stopped-reader, expired permit, literal input, resize, receipt, owner and
storage-fault regressions as well as the new component tests.

New actual cases prove:

- A closed helper remains on its original executable, in Starting, without native
  exec claim or Started, then reaches ECHILD at its original1500ms expiry.
- A small statically linked C ELF executes only after durable baseline acceptance.
  Overwriting its mutable source with invalid bytes after sealing does not change
  executed bytes. The program receives the exact UTF-8/NUL literal startup file;
  a renewal beyond the supplied original hard deadline refuses.
- Durability refusal, reader drop, SIGSTOPed reader, helper death and expiry during
  the durability callback all prevent native exec and Started, with stopped proof.
- A test-only guardian SIGSTOP after fixed exec permit/ACK lets the short native
  image finish before guardian observation. After resume there is an exec intent
  and eventual ECHILD, but no live-image proof and no Started callback.

Environment is retained in `wrk14-observed-environment.json`: Linux WSL2
6.18.33.2 x86_64, tmux3.4, rustc1.96.1, cargo1.96.1, Ubuntu GCC13.3.0,
24 reported processors. Cgroup CPU/memory limits were unavailable at the ordinary
v2 paths. Other agent builds/tests ran concurrently; this is not a dedicated-host
capacity/SLO claim. Root removed only an obsolete derived compile cache when disk
space became low; no source, evidence, work, quota or test limit was removed.

## Retained failures and their scope

All preceding `wrk14-observed-*` log files are retained, including compile/lint
iterations. They are development logs, not exact-source qualification snapshots.
No failed check is replaced or erased:

- The first actual closed-gate run sampled Waiting before a control round-trip
  proved the helper closed. It incorrectly required that stale sample to already
  be Starting. The implementation now reads current state after the receipt;
  the gate itself never released the native image.
- The next WIP run once reported `guardian exited before identity` at about1.5s.
  Its temporary fixture was not preserved, so the exact cause remains unknown.
  Later diagnostics preserve failed preparation roots and private test-only
  guardian bootstrap stderr. Subsequent actual matrix/full-suite checks passed;
  no permit, readiness timeout, scheduling retry or test serialization was widened
  to mask it. This observation remains a limitation of the evidence, not a claim
  that the historical cause was proved fixed.
- The first missed-exec test expected a new test hook that had not actually been
  inserted; it timed out waiting for the absent hook. Wiring the explicit
  test-support hook produced the deterministic positive refusal result. This
  changed the synthetic fixture, not the production exec-proof rule.

## Review map and remaining gates

`observed/executable.rs` owns immutable bounded image admission;
`observed/gate.rs` owns exact local peers, descriptor retention, physical closed
baseline and one-shot release; `observed/exec.rs` owns execveat and positive live
kernel evidence; `observed/mod.rs` owns opaque API/durability callbacks.
`local_ipc.rs` extracts the existing bounded packet/SCM_RIGHTS primitive without
broadening the reader protocol. `lib.rs`/`supervisor.rs` add the explicit path and
shared minimum-deadline calculation. `terminal_io.rs`/`reader_lease.rs` allow only
the private preexec output observation; ordinary input/control still require
Running. `terminal_output.rs` records actual EAGAIN for the local baseline check.

This checkpoint proves the stated synthetic component only. It does not qualify
a real Codex/harness, provider/model readiness, dynamic loader/tool dependencies,
startup counting, native producer durability/adoption, browser/cell agreement,
physical Unicode/alternate-screen renderer agreement, physical resize ordering,
Snapshot production/barriers, resynchronization, reconnect replacement, or
unknown-input resolution. Sealing the primary ELF does not seal dependencies.
After one-shot observer loss a new qualified baseline/producer protocol is still
required; no same-terminal implicit reattach is introduced. These remain explicit
WRK-14 follow-up gates, and no output Snapshot/capture claim is made here.
