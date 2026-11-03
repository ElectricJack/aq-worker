# B5a node controller evidence

Task `noble-summit-37.5`, claim epoch 2. This implements the authorized controller
seam and fake-backend lifecycle. It does not claim deployed sandbox execution
or review approval. The final source OID and review disposition are recorded in
the AQ submission/close receipt, avoiding a self-referential commit hash here.

## Scope decision and dependencies

Stacked on `aq/wise-cascade-63 c413cfe2` (B1, aq-admission 0.2.0). The guarded
pull brought in PR #12, and merge commit
`ff97d558d27e18f8bdf8eb57e2376a7205d94806` was pushed to this task branch.
That initial contract archive had SHA-256
`3c5054ebee1ecd87fc3380292830e650efcdd4d0beb43a779fd0cf9cd2eb64d7`.
The pre-review sync used `aq git pull --branch main`, bringing in `origin/main`
`303c5f3811fbc7df6cd582cf9e8fb131a8e94de8` (PRs #13 and #14) as merge
`1259ffe`. The B1 review revision replaces the archive with SHA-256
`352ed85f192a1ccbe116352a77a443504e8c5cd9023e7923961bcb5570ec7978`.
The review revision synced `origin/main` again at
`9a00f05142957accfdb66cc7a7f40fe9e563356e` (C3b template tooling), producing merge
`eabca498fc71b0fa5c51ab1194197e46573232f8`. This includes the review fix commit
`b78350a`. No B5a changes were made to the owned protocol or release archive.
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

Linux, Bash, assigned AQ worktree slot-3. Cargo uses the committed lockfile.
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
while an existing root must contain valid state. Failed journal writes poison
the invocation. A new boot retains all outstanding
reservations but cannot execute old acceptance or create intents. The recovery
fence is fixed for the invocation: even after explicit stop proofs clear all
inherited reservations, admission remains denied until restart. B5b owns any
future in-process reconciliation transition.

A pre-created or interrupted empty root is refused too. Only when the operator
knows this was a new initialization with no prior journal, reservations or
sandbox history, stop the process and remove the empty directory with `rmdir`
then restart (its parent must exist). `rmdir` must fail for any nonempty root;
never delete pending journals, private material or evidence to bypass this fence.
An empty root of unknown provenance may represent lost history and requires
reconciliation, not this new-initialization procedure.

`reserve` checks the actual backend's capabilities, exact advertised template
and harness version, sandbox count, estimated spend, preparation backlog,
CPU/memory, disk reserve, node/boot binding and deadline. Refusals are typed,
journaled and exposed in the local inventory. Preparation backlog conservatively
counts every reservation without positive stopped-writer proof whose phase is
not `Running`, including `Reserved`, ambiguous acceptance/create and `StopIntent`.
It is not a count of active preparation processes. `accept_with` persists reservation
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
agent execution authority. If create returns an untrusted/mismatched identity,
or recording its result fails, the durable `CreateIntent` may have no backend
ID. Capacity stays held. B5a cannot stop or destroy that unidentified object;
B5b must reconcile inventory using the persisted exact ownership/create intent.
The controller must not adopt an unvalidated returned identity just to permit
cleanup.

Positive matching backend stop proof releases sandbox/CPU/memory capacity;
unknown state, lost responses and backend failures retain it. Disk reservations
remain held until confirmed destruction. A controller-requested destruction additionally requires
caller-supplied retention/operator authority, durably recorded before the call.
An authoritative matching `Destroyed` observation from C1 may also release disk
without a local destroy intent: observing an already destroyed object requests
no deletion. The backend is trusted to attest destruction (including absence of
writers and disk), not merely an unknown/missing object. The external-destruction
regression test exercises this distinction without a controller destroy call. No automatic cleanup or replacement
is implemented here. Private local evidence is retained for B5b's retention work.

`aq-worker run --controller-config /absolute/node.json` consumes operator JSON:
`state_dir`, `node` (NodeConfig) and `docker_local` (existing DockerLocalConfig).
Enrollment remains the existing `--config` file. The configured advertisement
must name only digests allowed by that backend. The generic service registers,
holds the root and waits for shutdown; the CLI keeps the foundation heartbeat
alongside it. The shipped `UnavailableProvider` refuses before admission, pending
B2 integration. Its typed unavailability keeps the service/root lock and
foundation heartbeat alive with admission disabled (`GrantDenied`); shutdown
records draining. Other registration, startup and heartbeat errors still fail
with their actual diagnostic. Foundation credential rejection, protocol
incompatibility and Access rejection retain exit codes 3, 4 and 5 respectively;
other failures use 2. There is no hidden legacy or synthetic fallback. No claim is made
that an operator can execute a sandbox through this CLI before B2 is installed.

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

## Pre-review verification results

| Command | Observed result | Log |
| --- | --- | --- |
| `cargo test --locked --workspace capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason` | 1 matched test passed | `B5a-evidence/acceptance-capacity-final.log` |
| `cargo test --locked --workspace journal_records_intent_before_backend_create` | 1 matched test passed | `B5a-evidence/acceptance-journal.log` |
| `cargo test --locked -p aq-worker --lib controller::` | 17 passed after main sync | `B5a-evidence/controller-synced.log` |
| `cargo test --locked -p aq-worker --lib native::runner::` | 21 passed after main sync | `B5a-evidence/native-synced.log` |
| `cargo test --locked -p aq-worker --bin aq-worker run_requires_explicit_controller_configuration` | 1 passed | `B5a-evidence/cli.log` |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | passed after main sync | `B5a-evidence/clippy.log` |
| `cargo fmt -p aq-worker --check` | passed | `B5a-evidence/fmt-worker.log` |
| `cargo fmt --check` | passed after main sync | `B5a-evidence/fmt-workspace.log` |
| `cargo fmt --all --check` | cannot resolve excluded vendor packages from nested AQ slot | `B5a-evidence/fmt-all.log` |
| `git diff --check` | passed | terminal output |

The `fmt --all` limitation matches the B1 revision evidence: Cargo resolves an
excluded vendored package's workspace to the outer checkout in this nested AQ
slot. Workspace-member formatting and the changed worker package both pass.
No full workspace test suite, real sandbox execution, provider CLI execution,
or GitHub Actions run is claimed. Exact named workspace filters each matched
one real test; the zero-match outputs from unrelated binaries are not counted.

The pre-review source was verified at merge `1259ffe`; publication at `42f828ce`
changed only evidence and the execution ledger. The review revision below verifies
new source at merge `eabca498`. `environment.log` records the
source and toolchain checkpoints. The final pushed OID, PR, review ID, content
hash and reviewer disposition belong to the AQ review/task receipt. Passing
checks are not a substitute for the requested adversarial review.

## Review revision verification

`revision-red-service.log` reproduces premature service completion before a
heartbeat; `revision-red-cli.log` reproduces exit 2 for credential revocation
instead of 3. Both are behavioral failures, not compile errors. After the fix
and latest-main merge, the affected controller and CLI suites pass:

| Command | Observed result | Log |
| --- | --- | --- |
| `cargo test --locked -p aq-worker --lib controller::` | 19 passed | `B5a-evidence/revision-controller-synced.log` |
| `cargo test --locked -p aq-worker --bin aq-worker` | 4 passed | `B5a-evidence/revision-cli.log` |
| `cargo test --locked --workspace capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason` | 1 matched test passed | `B5a-evidence/revision-acceptance-capacity.log` |
| `cargo test --locked --workspace journal_records_intent_before_backend_create` | 1 matched test passed | `B5a-evidence/revision-acceptance-journal.log` |

The final `cargo clippy --locked --workspace --all-targets -- -D warnings`,
`cargo fmt -p aq-worker --check`, `cargo fmt --check`, and `git diff --check`
all passed. `B5a-evidence/revision-checks.json` records explicit exit codes for
formatting and diff checks (including silent successful commands) and AQ gate
exit codes for tests/Clippy, with paths to full command logs. The earlier
`fmt --all` nested-vendor limitation remains documented above.

The heartbeat regression uses a concurrent fake heartbeat future, verifies three
progress ticks while registration remains disabled, and checks durable refusal
and exclusive root ownership. It makes no HTTP/provider compatibility claim.
The actual-registration-error regression verifies a credential failure is still
fatal. The native runner is unchanged from its pre-review 21-test verification.

## Round 1 review dispositions

Review `rev-grand-meadow` revision 1 produced two blocking findings and six nits.
Author fixes do not constitute review approval. All eight dispositions follow:

- `cmt-a712055bcdaf` — **fixed**: controller/service.rs handles only typed ProviderUnavailable as nonfatal, persists GrantDenied, retains the root and waits for shutdown. tests.rs:unavailable_provider_keeps_service_and_heartbeat_alive_without_admission verifies concurrent heartbeat progress, durable refusal and exclusive root ownership. README and B5a.md document the required config and disabled admission.
- `cmt-4a8b3c9b15a1` — **fixed**: main.rs:run_failure retains the actual error chain and maps wrapped CallError through exit_for. run_errors_preserve_diagnostics_and_foundation_exit_codes covers rejection codes 3/4/5, other failures 2, contextual diagnostics and an unrelated root-lock failure. Actual registration failures still propagate; tests.rs covers invalid credentials.
- `cmt-685fb608874a` — **rebutted**: C1 backend/types.rs:InventoryEntry::writer_proof explicitly attests Destroyed only with zero writers and no disk. Observing authoritative external destruction does not request deletion. Controller::destroy still requires and journals caller authority before calling the backend. lifecycle_tests.rs:authoritative_external_destruction_releases_disk_without_requesting_deletion demonstrates external authorized deletion followed by inspection, with no controller destroy authorization/call. B5a.md clarifies the trust boundary.
- `cmt-e9006556e4ca` — **fixed**: B5a.md now states recovery fencing lasts the whole invocation even after inherited reservations receive stop proof. Restart or future B5b reconciliation is required; this patch does not invent an in-process recovery transition.
- `cmt-d861d50dcfd7` — **fixed**: B5a.md documents stopped-process rmdir recovery only for a known-new empty initialization with no prior history. Nonempty roots and roots of unknown provenance must be preserved for reconciliation.
- `cmt-d60d8ab37a2a` — **fixed**: B5a.md states that untrusted create identity or failed persistence can leave CreateIntent without a backend ID, with capacity held and B5a stop/destroy unable to target it. B5b must reconcile exact persisted ownership; an unvalidated returned identity is not adopted.
- `cmt-2b791affcbc2` — **fixed**: B5a.md defines preparation backlog as all unstopped reservations outside Running, explicitly including Reserved, ambiguous intents and StopIntent; it is conservative admission accounting, not a process count.
- `cmt-ac98fd066ef2` — **fixed**: B5a-evidence/revision-checks.json records explicit subprocess exit codes for worker formatting, workspace-member formatting and git diff --check alongside their logs; AQ gate exit codes and exact command logs cover the test and Clippy runs.
