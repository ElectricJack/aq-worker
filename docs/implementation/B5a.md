# B5a node controller evidence

Original task `noble-summit-37.5`, claim epoch 2; revised by `wise-vault-78`. This implements the authorized controller
seam and fake-backend lifecycle. It does not claim deployed sandbox execution
or review approval. The final source OID and review disposition are recorded in
the AQ submission/close receipt, avoiding a self-referential commit hash here.

## Scope decision and dependencies

Stacked on `aq/wise-cascade-63 c413cfe2` (B1, aq-admission 0.2.0). The guarded
pull brought in PR #12, and merge commit
`ff97d558d27e18f8bdf8eb57e2376a7205d94806` was pushed to this task branch.
That initial contract archive had SHA-256
`3c5054ebee1ecd87fc3380292830e650efcdd4d0beb43a779fd0cf9cd2eb64d7`.
Final sync used `aq git pull --branch main`, bringing in `origin/main`
`303c5f3811fbc7df6cd582cf9e8fb131a8e94de8` (PRs #13 and #14) as merge
`1259ffe`. The B1 review revision replaces the archive with SHA-256
`352ed85f192a1ccbe116352a77a443504e8c5cd9023e7923961bcb5570ec7978`.
No B5a changes were made to the owned protocol or release archive.
The frozen 0.1.11 contract remains for installed legacy native consumers.

Supervisor message `msg-e374e43f61d741aa8022af17da124d39` confirmed that B2's
initial issuance and registration HTTP routes are not published. It explicitly
authorized implementing and closing B5a on one typed injected provider seam,
with fake-provider tests; full HTTP acceptance-to-runner composition follows
B2 and must not be invented here. The seam consumes published registration
and DelegatedPermit types. The associated initial request is opaque to the
controller. Runner material packaging also remains opaque to this controller,
so it does not accidentally publish a B6 payload contract.

The original native routes remain in `native/http/control.rs` and
`native/phase_profile_shared/http_profile.rs`: profiled poll and profiled-v2
proposal/accept. The controller's durable acceptance callback is the local
composition point; B2 must connect those actual observations and its published
issuance transport. No worker-local HTTP request/response types or endpoint
names are introduced. The shipped unavailable provider explicitly refuses
registration rather than falling back to legacy execution.

Plan: `docs/superpowers/plans/2026-11-02-b5a-node-controller.md`, committed as
`afbf799` before source implementation. Source baseline after prerequisite merge:
`ff97d558d27e18f8bdf8eb57e2376a7205d94806`.

## Verification environment

Original verification: Linux, Bash, AQ slot-3. Revision verification: Linux, Bash, AQ slot-0. Cargo uses the committed lockfile.
The installation's `aq test` is pytest-only; the committed subprocess adapter
`B5a-evidence/test_cargo.py` runs each exact Cargo command under AQ's gate with
`CARGO_BUILD_JOBS=2`, writing full output to the named log. No real backend,
cloud API, credentials or provider CLI is exercised by the fake tests.

## Limits retained by design

- B2 owns authenticated issuance and registration HTTP integration. This change
  does not claim compatibility with an unpublished endpoint.
- B6 owns the fixed in-sandbox runner and its private-material encoding. A
  preparation permit allows that runner to prepare, never to launch an agent.
- B5b owns restart reconciliation/re-adoption and automatic preservation/sweeps.
  Old unresolved reservations remain held, and a new boot cannot relaunch them.
  The recovery fence lasts for the whole invocation: stopping inherited attempts
  does not clear it. Only reopening re-evaluates unresolved reservations.
  If create returns a mismatched identity, or persisting its validated identity
  fails, the snapshot can remain at CreateIntent without a sandbox identity.
  Stop/destroy then refuse and reservations remain held for B5b reconciliation;
  the controller never targets an unvalidated backend identity for cleanup.
- Local spending is conservative cumulative estimated cold-start admission
  accounting, not provider billing or a promise to enforce actual cloud costs.
- No design/spec documents are edited. The existing rootless Docker backend is
  reused, with its shared-kernel isolation limitations unchanged.

## Implementation and integration seam

`controller::Controller<B, P>` owns a 0700 state root and an exclusive directory
lock. Descriptor-relative, no-follow opens create 0600 files; writes sync the
file and directory before a backend effect. A versioned checksummed snapshot
rejects unsupported, malformed, foreign-node, replaced-root or symlink state.
The parent directory must exist; first initialization creates an absent root,
while an existing root must contain valid state. A crash before initial journal
persistence or a provisioning tool that pre-creates the root can leave an empty
0700 directory that is refused on the next open. With all controller processes
stopped, an operator may inspect that exact root (including hidden entries) and
remove it with `rmdir` only if it is completely empty: no journal.json,
journal.pending, private material or other entries. Restart then creates it.
Preserve any nonempty or uncertain root for reconciliation; do not recursively
remove state to bypass validation. Failed journal writes poison
the invocation. A new boot retains all outstanding
reservations but cannot execute old acceptance or create intents.

`reserve` checks the actual backend's capabilities, exact advertised template
and harness version, sandbox count, estimated spend, preparation backlog,
CPU/memory, disk reserve, node/boot binding and deadline. The inventory's
`preparing` backlog conservatively counts every unstopped, non-Running attempt,
including Reserved and unresolved StopIntent; it is not only active preparation. Refusals are typed,
journaled and exposed in the local inventory. `accept_with` persists reservation
and acceptance operation ID before calling the native acceptance future. An
error retains the reservation and the ambiguous acceptance intent. The trusted
callback must return `Ok` only for a validated positive native acceptance;
negative observations, uncertainty and transport errors must return `Err`.

`launch` records create intent with exact digest/ownership/resources, records
the returned backend ID, and records issuance intent before invoking the opaque
provider. A preparation permit must match node, boot, attempt, generation and
hard deadline and be currently valid. The permit and frozen manifest are saved
in separate private files, with only permit/sandbox IDs in the snapshot. The
provider supplies the opaque runner payload over C1's private channel; it is
responsible for packaging the same validated permit and manifest. Material
handoff and start each have a preceding durable intent. This does not grant
agent execution authority.

Positive matching backend stop proof releases sandbox/CPU/memory capacity;
unknown state, lost responses and backend failures retain it. Disk reservations
remain held until confirmed destruction. Destruction additionally requires
caller-supplied retention/operator authority. A Destroyed-kind proof without a
durable authorized DestroyIntent (or an already authorized Destroyed record)
poisons the invocation and preserves disk reservations, including proofs from
stop/inspect. Already authorized destruction remains inspectable. No automatic cleanup or replacement
is implemented here. Private local evidence is retained for B5b's retention work.

`aq-worker run --controller-config /absolute/node.json` consumes operator JSON:
`state_dir`, `node` (NodeConfig) and `docker_local` (existing DockerLocalConfig).
Enrollment remains the existing `--config` file. The configured advertisement
must name only digests allowed by that backend. With an installed provider, the
generic service registers, holds the root and waits for shutdown. The CLI uses
`run_with_heartbeat`: when the shipped `UnavailableProvider` returns typed
`ProviderUnavailable`, it logs the distinct B2 refusal, retains the root lock and
continues foundation probe/heartbeat/reconnect until shutdown or a terminal
connector error. Inventory remains unregistered/GrantDenied during that interval;
no reservation or backend execution is permitted. The foundation heartbeat is
connectivity evidence only and grants no sandbox authority. Other controller
errors remain fatal. Shutdown drains the controller; the signal watcher is aborted
when run completes. Startup and runtime errors print their actual error chain.
Connector errors retain exit codes 3 (revoked/invalid credential), 4 (incompatible),
5 (Access rejected), and 2 (other errors, including startup/config/state errors).
There is no hidden legacy or synthetic execution fallback. An operator cannot
execute a sandbox through this CLI before B2 is installed.

`run-native` now opens and validates an existing journal. It then reports that
historical state needs B5b reconciliation before any new admission, preserving
the existing journal's recovery fence. Orphan materials cannot initialize a new
journal. Fresh roots retain the existing native behavior.

## Test-first record

- `red-controller.log`: 12 behavior failures against the scaffold, followed by
  `green-controller.log`: the same 12 tests passed.
- `red-native.log`: existing state failed at unconditional journal creation,
  before adding the validated open path.
- `red-service.log`: 13 passed, two intended service assertions failed before
  implementing shutdown waiting and unavailable-provider refusal.
- `red-capacity.log` and `red-operation-intent.log` record intermediate compile
  failures; they are not claimed as behavioral red evidence.

- `red-intent-assertion.log`: the backend lifecycle test failed because registration
  operation ID was not yet durable; the implementation now journals it before
  the provider call and journals destruction authority before destroy.

## Original verification results (42f828ce)

| Command | Observed result | Log |
| --- | --- | --- |
| `cargo test --locked --workspace capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason` | 1 matched test passed | `B5a-evidence/acceptance-capacity-final.log` |
| `cargo test --locked --workspace journal_records_intent_before_backend_create` | 1 matched test passed | `B5a-evidence/acceptance-journal.log` |
| `cargo test --locked -p aq-worker --lib controller::` | 17 passed after main sync | `B5a-evidence/controller-synced.log` |
| `cargo test --locked -p aq-worker --lib native::runner::` | 21 passed after main sync | `B5a-evidence/native-synced.log` |
| `cargo test --locked -p aq-worker --bin aq-worker run_requires_explicit_controller_configuration` | 1 passed | `B5a-evidence/cli.log` |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | passed after main sync | `B5a-evidence/clippy.log` |
| `cargo fmt -p aq-worker --check` | original empty log has no recorded exit code; superseded below | `B5a-evidence/fmt-worker.log` |
| `cargo fmt --check` | original empty log has no recorded exit code; superseded below | `B5a-evidence/fmt-workspace.log` |
| `cargo fmt --all --check` | cannot resolve excluded vendor packages from nested AQ slot | `B5a-evidence/fmt-all.log` |
| `git diff --check` | original terminal-only assertion; superseded below | no archived exit code |

The `fmt --all` limitation matches the B1 revision evidence: Cargo resolves an
excluded vendored package's workspace to the outer checkout in this nested AQ
slot. Workspace-member formatting and the changed worker package both pass.
No full workspace test suite, real sandbox execution, provider CLI execution,
or GitHub Actions run is claimed. Exact named workspace filters each matched
one real test; the zero-match outputs from unrelated binaries are not counted.

The original source was verified at merge `1259ffe`; original publication edits
were this evidence and the execution ledger only. Revision 2 changes follow below. `environment.log` records the
source and toolchain checkpoints. The final pushed OID, PR, review ID, content
hash and reviewer disposition belong to the AQ review/task receipt. Passing
checks are not a substitute for the requested adversarial review.

## Revision 2 changes and verification

Revision task `wise-vault-78` merges the reviewed source `42f828ce` into the
assigned branch, preserving reviewed history. The injected PermitProvider
request/payload seam and the B2/B5b/B6 scope decision are unchanged.

- `cmt-a712055bcdaf` fixed in `controller/service.rs:run_with_heartbeat` and
  `main.rs:Cmd::Run`. The regression uses the real foundation HTTP loop with a
  local contract fake, requires two heartbeats across an interval after B2
  registration refusal, checks the root remains locked, then verifies draining
  and no backend effects. Generic `run` still returns its registration error.
- `cmt-4a8b3c9b15a1` fixed in `main.rs:run_error` and `Cmd::Run`. Tests check
  context/cause preservation, every connector exit-code class, real missing
  enrollment and malformed controller JSON failures, and that unrelated
  controller failure is fatal while heartbeat is pending.
- `cmt-685fb608874a` fixed in `lifecycle.rs:record_proof`. FakeBackend faults
  inject Destroyed proofs through stop and inspect; both tests verify poisoning
  and retained disk. Existing authorized destroy remains covered.
- `cmt-e9006556e4ca` fixed by documenting the invocation-long recovery fence
  above; no automatic reopening or reconciliation is claimed.
- `cmt-d861d50dcfd7` fixed by the empty-root operator procedure above.
- `cmt-d60d8ab37a2a` fixed by documenting CreateIntent without a recorded
  identity under B5b limits. An unvalidated identity is never a cleanup target.
- `cmt-2b791affcbc2` fixed by defining backlog as unstopped and non-Running.
- `cmt-ac98fd066ef2` fixed by fresh formatting/whitespace checks with explicit
  command, log and exit-code records. Historical empty logs alone are not proof.

Revision checks and results are recorded in
`B5a-evidence/revision-checks.json`; each named log has a sibling JSON command
receipt with an explicit exit code, emitted by `test_cargo.py` under `aq test`.
No whole-workspace test suite, real sandbox, deployed B2 compatibility or
approval is claimed. Final source OID and review content hash are recorded in
the AQ submission and task close receipts.

The initial revision controller run had 21 passes and one new test failure:
`heartbeat_does_not_hide_controller_journal_failure` expected Draining after
root replacement. Since a replaced root cannot be written, the correct inventory
is GrantDenied. The test now checks the exact original root-change error and the
retained fence. `revision-controller-initial.log` and its exit-101 JSON receipt
preserve that failure; `revision-controller.log` records the corrected run.

Final sync uses fetched `origin/main` at
`9a00f05142957accfdb66cc7a7f40fe9e563356e` (already present; merge was up to date).
`origin/dev` is absent. Supervisor message
`msg-7bde0584f9dd4e859e014e74330896d3` explicitly waives the generic dev instruction
and requires main for this repository; no dev branch was created or main pushed.

| Revision command (Cargo commands run through AQ adapter) | Result | Receipt/log basename in B5a-evidence |
| --- | --- | --- |
| `cargo test --locked -p aq-worker --lib controller::` | 22 passed | `revision-controller` |
| `cargo test --locked -p aq-worker --bin aq-worker run_` | 2 passed | `revision-cli` |
| `cargo test --locked -p aq-worker --test controller_cli --test client --test foundation_lifecycle --test backend_conformance` | 33 passed | `revision-area` |
| `cargo test --locked -p aq-worker --lib native::runner::` | 21 passed | `revision-native` |
| `cargo clippy --locked -p aq-worker --all-targets -- -D warnings` | exit 0 | `revision-clippy` |
| `cargo fmt -p aq-worker --check` | exit 0 | `revision-fmt-worker` |
| `cargo fmt --check` | exit 0 | `revision-fmt-workspace` |
| `git diff --check HEAD^` | exit 0 | `revision-whitespace` |

Exact AQ invocation for each Cargo check is
`B5A_COMMAND='<Cargo command above>' B5A_LOG=docs/implementation/B5a-evidence/<basename>.log B5A_RESULT=docs/implementation/B5a-evidence/<basename>.json aq test docs/implementation/B5a-evidence/test_cargo.py -q`.
The three direct formatting/whitespace commands also appear with explicit exit
codes in `final-checks.json`. The corrected final suites total 78 passing tests.

Archived revision Cargo logs omit blank trailing lines. The final whitespace
check compares the complete revision to its parent (`git diff --check HEAD^`),
including the newly added evidence files.
