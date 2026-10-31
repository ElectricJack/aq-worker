# Worker 0.2.0 recovery candidate

This implementation qualifies a bounded Linux descendant-ownership profile for
runtime-process restart. It does not establish general hostile-code containment
or whole-container restart recovery. Public wire packages remain unchanged:
foundation `aq-protocol` 0.1.0 and execution `aq-worker-protocol` 0.1.1. Worker
journal format advances from 1 to 2 with a conservative migration.

## Supported ownership boundary

`linux-subreaper-guardian-v1` requires Linux `PR_SET_CHILD_SUBREAPER`, pidfds,
`waitpid(__WALL)`, readable own-process procfs child lists, abstract Unix sockets,
`SO_PEERCRED`, and suspend-aware `CLOCK_BOOTTIME`. Startup probes these facilities
and refuses execution if they are unavailable. Foundation connectivity retains
its version-1 meaning. No cgroup delegation, root privilege or new service is
required. The guardian is a dedicated single-threaded process and cannot be PID
namespace init. Container deployments should use Docker `--init` / Compose
`init: true` when the controller runs as a child, for orphan guardian reaping.

Each guardian owns one launch and all its kernel descendants. It becomes a
subreaper before creating an execution writer. A double-forked child changing
session/process group remains within that ancestry. `/proc/.../children` is used
to discover termination targets; pidfds address those exact children. The
single-threaded guardian does not reap between discovering a child and opening
its pidfd, preventing that child PID from being reused during targeting.

An empty process scan, initial-child exit, process-group disappearance or missing
PID never establishes stopped writers. The final proof is the same trusted
subreaper receiving kernel `ECHILD` from `waitpid(-1, __WALL | WNOHANG)` after
reaping every adopted child. `__WALL` includes clone children. A durable receipt
binds that proof to guardian nonce, PID/start time, machine boot, PID namespace,
node/boot, attempt/session/generation, resolved spec digest and launch ID.

The boundary excludes writers launched through external services outside this
ancestry, hostile ptrace/process-parent manipulation, tampering with private
runtime files/control, and a malicious replacement for the pinned guardian
binary. It is cooperative process ownership, not an OS sandbox. Such execution
configurations must not claim this profile. Future real harnesses need their own
compatibility evidence and must refuse unsupported ownership requirements.

## Durable launch handshake and ambiguous boundaries

1. The worker durably records a unique launch ID before spawning its guardian.
2. Immutable per-launch binding, fixture input and private token reach file and
   directory fsync. The guardian obtains a one-shot claim, enables subreaping,
   binds its endpoint and writes its complete identity before waiting for Start.
3. The worker verifies that identity and persists it in the journal. It then
   persists Start intent before sending the authenticated Start operation.
4. The guardian validates a fresh clock against the matching grant, records the
   action, checks time again after durability work, and forks the writer once.
5. It records the root process evidence and returns the receipt. The worker
   persists running evidence. Either side may crash around these separate steps.

This ordering closes the spawn/PID-record gap: a surviving guardian is
independently discoverable even if the worker never recorded the root PID.
Absent durable Start intent in this new profile proves no Start could have been
sent; PID absence is not the reason. Once Start intent exists, missing guardian
or missing final receipt remains `stop_uncertain`. There is no blind relaunch.

Control uses a private 0600 token outside the execution grants. The abstract
socket verifies peer UID/PID and the full launch/attempt identity; a stale session
cannot address another guardian. Tokens do not appear in the journal, public
receipts, process arguments or result reports. The runtime lock is explicitly
verified `CLOEXEC`; the guardian receives neither that lock nor the node config
or credential. Socket connects are nonblocking, messages are bounded at 16 KiB,
and reads/writes have absolute whole-message deadlines. Trickle traffic cannot
reset their budgets or indefinitely occupy the independent lease loop.

## Lease, stop and restart behavior

The guardian enforces its own grant using CLOCK_BOOTTIME, including while the
worker is dead or disconnected. Renewal requires a still-live prior deadline,
matching binding, increasing grant sequence and a fresh check after request
reception. A stale timestamp captured before a slow read cannot admit Start or
Renew. Late authority cannot revive stopped execution.

Cancellation, drain, expiry or recovery requests stop writers. The guardian sends
TERM to owned children, escalates to KILL after 100 ms and caps its normal stop
attempt at five seconds. These termination budgets use independent raw
CLOCK_BOOTTIME, so a regressed/unreadable authority test clock cannot freeze
escalation. Ordinary I/O errors also trigger best-effort emergency termination;
without a durable final proof, uncertainty remains. This is not a hard real-time
claim under a hung kernel/filesystem or resource starvation.

A runtime restart creates a new boot ID, opens the exclusive journal, accounts
for protected reservations, and stops surviving old guardians before outward
reconciliation and registration. A stopped immutable report can be replayed
without renewing agent execution authority. Existing live sessions are not
reattached or resumed; any future supported reattachment requires explicit
current authority and adapter evidence. New execution uses a new attempt and
session. Drain remains durable across restart and prevents admission.

A reservation that expires before launch is reported `NotStarted`/`Expired`.
For uncertain acceptance, the worker durably retries an original-node/boot/offer
bound decline. Its acknowledgement proves the offer was never granted and
fences a racing acceptance. If the daemon refuses decline because acceptance
committed, the worker submits the same immutable nonstart report. Other failures
preserve the report and protected bytes; no execution starts on a delayed grant.

If a guardian dies, its previously durable kernel-stop receipt can still prove
that launch stopped. Without that receipt, missing guardian/root/descendant PIDs
cannot release resources. Restart in a different PID namespace cannot contact
an old live guardian. Entire container/PID1 death commonly destroys the old
namespace; absent an already durable stop receipt, that remains uncertain and
requires stronger containment evidence or an explicit scoped recovery decision.
This candidate does not automatically certify container restart or host loss.

## Journal compatibility

Format 1 is accepted and atomically rewritten as format 2. Retired entries and
known stopped immutable report bytes keep their existing disposition. Legacy
active/uncertain launches have no new guardian proof: missing new fields never
imply that Start did not happen. They remain protected even if their PID is gone.
An exact frozen format-1 reader from worker commit
`cf87edffa3384a08f5bb0319b4f92ffc2a26b145` refuses the new state without modifying
it. Its retained source SHA-256 is
`7060062e5f57b33d1e5cfa5b6c422de8974576b6aa961dda38c64d07b05a5673`.
The product harness separately checks the historical binary/image downgrade.

## Standalone evidence and repeatable checkpoints

Linux 6.18.33.2-microsoft-standard-WSL2 x86_64; rustc/cargo 1.96.1.
Eleven `tests/guardian.rs` process regressions cover:

- Actual process death at grant, launch-intent, guardian-spawn, guardian-identity,
  Start-intent, Start-send-before-receipt and Start-receipt boundaries.
- A double-forked `setsid` writer continuing after controller death, followed by
  live-guardian termination and kernel stop proof.
- Initial root exit with a live descendant; root exit alone cannot report success.
- Guardian loss retaining reservations even after a test harness kills known PIDs.
- Independent expiry with a dead runtime; unreadable/regressed clock and ignored
  TERM still escalate through raw-clock KILL and reap.
- Stalled Start/Renew requests crossing expiry, stale identity rejection and slow
  trickle traffic while the independent watchdog expires the writer.
- Lock-descriptor noninheritance, secret omission and capability refusal.

Four migration/drain regressions cover format-1 retired/stopped reports, active
legacy uncertainty, old-reader refusal and durable drain. Existing foundation,
wire, admission and real-child HTTP replay tests remain in the component suite.
Commands and results are retained in `worker-recovery.tests.log`:

```
cargo test --workspace --all-features --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

`aq-worker-test-host` is a private fixture host with
`required-features = ["test-support"]`; it is excluded from normal production
`cargo build --bins`. Actual daemon/image fault acceptance belongs to Agent Q.
Its matrix must pin the final source archive and retain its own correlation IDs,
images, outcomes and explicit uncertainty. This component report does not close
that distributed gate by itself.

Private injection interface (disabled unless explicitly configured):

- `run-execution --fault-checkpoint NAME --fault-action crash|pause`. Names are
  `grant-durable`, `launch-intent-durable`, `guardian-spawned`,
  `guardian-identity-durable`, `start-intent-durable`,
  `start-sent-before-receipt`, `start-receipt-durable`, `report-durable`.
- Markers: `state/faults/NAME.json`; pause resumes after `NAME.release`; crash exits
  86. Process-kill tests wait for these barriers before sending SIGKILL.
- `AQ_FIXTURE_HOLD=1`, `AQ_FIXTURE_DESCENDANT=double-fork-setsid` (or
  `double-fork-setsid-root-exit`), `AQ_FIXTURE_IGNORE_TERM=1` exercise real writers.
- `AQ_WORKER_CLOCK_OFFSET_FILE` / `AQ_GUARDIAN_CLOCK_OFFSET_FILE` read monotonic
  u64 offsets. Mount their directory and atomically replace files. These are
  labelled simulated-time evidence, not target-host physical-suspend proof.
- Public metadata: `state/launches/ID/guardian-{launch,identity,state}.json`,
  `descendant-counter` and journal `stopped_proof`. `control.token` is private and
  must never be swept into qualification artifacts. `recovery_hold` reports
  protected and available resource arithmetic without token values.

Guardian-loss recovery without stronger kernel ownership, reattachment,
external-service writers, production harnesses, source retention, actual host
suspend/power-loss and product scale/fairness remain outside this profile.
