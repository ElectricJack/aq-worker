# WRK16A continuation history and source proposals

Status: pure/data-only source qualified on the worker0.5/FORMAT12 candidate.
Actual physical composition and production continuation remain pending.
This checkpoint does not qualify physical parking or central retention.

The sibling `continuation` module owns bounded data and finite proposals for a
single synthetic local-parking transaction. It reuses CON03 directly and the
existing source-transfer state machine. It cannot construct live execution,
stopped-workspace, coordinator-hold, retention or resumed-launch authority.
Native journal/root versioning and actual fsync/host ownership remain with their
existing owners. FORMAT11 contains no continuation field; insertion requires an
explicit new FORMAT12 rather than a compatible-looking default field.

`ParkingIntent` retains the exact original CON03 suspension/checkpoint pair,
explicit fake-hold marker, execution-record/host-intent/spec digests, source and
workspace association, actual directory device/inode pins, original disk and
source limits, fixed distinct operation identities and intent revision. The
physical bridge must collect these from actual retained owners before fsync;
caller-provided matching JSON is insufficient. A separate stop anchor refers to
the actual native stopped revision and parking record revision. The live token
also retains the true private stop proof and exact committed record digest; a
history record never reconstructs that token.

The source history is capped at 256KiB inside the existing root journal limits.
Its only Git operations are checkpoint and export. Each original request is
persisted before exchange, and its decoded projection must retain the original
result. Lost-response checkpoint replay preserves the saved UUID/head/recipe,
even when that same operation has advanced Git's workspace generation. It must
not recompute a new request from a changed head. No source work is proposed
without a stopped anchor. Original directory pins and descriptor association
must be rechecked by the physical owner around every source read/exchange.

Disk accounting retains observed stopped-workspace bytes plus bounded actual
export bytes against the original reservation. Unknown measurements or either
pending Git operation cannot satisfy the completion predicate. Both checkpoint
and export must have validated terminal results. The public Git artifact UUID
and canonical manifest digest remain distinct from the transport UUID and raw
manifest byte digest. Only predecessor Checkpoint upload is represented; no
successor Base relabeling/import/launch is implemented.

`source` returns cloned history proposals; `transfer` constructs the existing
bounded upload projection and validates acknowledgments using the extracted
shared finite helpers. A caller must fsync the proposal through the retained
native owner before executing the corresponding I/O. These functions perform no
I/O and do not authenticate a coordinator response. A final transfer receipt is
still a claim until the actual authenticated coordinator owner verifies it.
The future fake coordinator must explicitly retain source and summary and
supply effects reconciliation; an empty operation list is never treated as
reconciliation. Protected disk remains charged after upload or report.

Parent-approved policy: Missing summary blocks retained/migratable suspension
and protected-disk release, but permits local checkpoint/export and freeing
proven idle compute. `settled_source_claim` requires both validated Git results
and bounded observed source bytes; native capacity additionally requires its
actual stopped proof/anchor and exact source association. The separate
`completed_source_claim` adds the Supplied summary prerequisite for migration.
Neither predicate establishes central retention or reconciled effects. Missing
is never silently converted to Supplied. No production capability, ordinary
Suspended result, or new launch is introduced.

Parent approved an explicit FORMAT12 candidate in a separate worker0.5 branch,
under the native schema owner's final ownership. Frozen worker0.4/FORMAT11 must
not be edited or reinterpreted.

Initial gate scope: focused pure tests and affected strict checking only. Pure
vectors cover no-stop/no-result refusal, both required source results,
fixed-ID/head replay, budget uncertainty/overflow, projection origins, exact
predecessor/boot binding and no Base relabeling. This is separate from the later
explicitly authorized disposable actual tmux + pinned aq-git + synthetic
coordinator composition. There is no provider, canonical agent, SQL, public
coordinator route or real central enforced hold in this slice.

Executed first pure qualification using the parent-released short Target2 lease,
two jobs, offline, incremental0 and dev/test debug0:

- `cargo test -p aq-worker --offline --lib continuation::tests`:5 passed,
  164 filtered,0 failed/ignored;0.01s tests after11.14s compilation.
- `cargo clippy -p aq-worker --offline --lib -- -D warnings`: passed10.68s.
- Affected rustfmt and diff whitespace check passed.

Target2 was released immediately after these gates. This gate is against the
independent sibling modules on worker0.3.4 baseline; it does not qualify the
separate worker0.5/FORMAT12 integration, private live stopped-workspace bridge,
actual tmux/Git, coordinator exchange or capacity projection. The five tests use
explicitly synthetic history/summary/source records and do not mint proof.

## Finite journal advancement follow-up

`ParkingHistory::digest` uses the existing canonical session-protocol digest.
`ParkingHistory::validate_advance` accepts an exact idempotent replay or one
finite source/upload step. It preserves original intent/stop/source facts and
requires checkpoint/export requests before their results, keeps pending chunk
UUID/recipe stable, and advances only that exact acknowledged range. A newly
pending range must follow the shared planner's deterministic bounded next-range
recipe. Neither matching JSON nor this validator supplies actual response,
filesystem, stop or coordinator authority. Native `parking_commit` must still
hold the opaque physical owner, compare the current digest and fsync the update.
The actual stopped anchor is inserted only by the separate physical bridge;
this finite helper cannot introduce/change it.

The focused follow-up tests reject bypassed fsync stages, changed intent/root/
stop pins, offset jumps without pending requests, replaced pending UUIDs and
shortened next-range recipes. They also exercise the valid one-step sequence
and stable digest/idempotent replay. Upload completion is required before even
a structurally valid source-retention receipt can appear in history.

Follow-up Target2 gate:7 pure continuation tests passed in0.02s after11.04s
compilation,164 filtered and0 skipped/failed. Affected `--lib` Clippy first
rejected two unnecessary unwraps; explicit absence errors replaced those calls
and strict rerun passed5.54s. No physical, coordinator or native integration gate
was run. Target2 was released after the focused gate.

The subsequent predicate hardening makes the public settled/completed methods
validate the entire history themselves rather than relying on every caller to
remember a prior validation. Retention shape validation uses a private finite
facts helper to avoid recursive validation. A direct malformed-base/operation-ID
regression accompanies this change. Its initial source freeze preceded the physical-bridge owner's gate; that owner
subsequently reported eight focused tests passed0.06s on the candidate. Our
later eleven-test gate below also executes this vector. The earlier seven-test
result alone does not cover it.

## Synthetic retention profile fixture (data-only qualification)

The new test-only `synthetic_retention` owner implements the explicitly approved
synthetic-retention-profile-v1. It is not a production TransferClient,
coordinator hold, summary/effects owner or frozen extension of CON03 semantics.
It admits the original predecessor transfer and actual bounded chunks, verifies
raw manifest/bundle SHA and the exact public Git manifest, then fsyncs raw wire
members before allocating a final source-retention claim. Canonical Git manifest
identity and raw manifest SHA remain separate. The Source receipt counts actual
raw manifest plus bundle bytes, never the expanded worktree estimate.

Summary retention checks the original exact Checkpoint claim, writes canonical
SummaryClaim UTF-8 JSON and pins that digest and complete serialized byte count.
The explicit candidate parking profile additionally caps those canonical bytes
against the original declared suspension budget, including context and pending
references. It rejects over-budget input without truncation. This does not
change or reinterpret CON03 text validation. Missing yields no checkpoint receipt.
The owner allocates artifact/receipt IDs once, preserves the original retention
interval on replay, and rejects changed predecessor, continuation, report,
uploader identity, source, summary or expired access. These associations remain
inside the explicit synthetic profile because CON03 receipt wire fields contain
an attempt Binding digest, not an independent continuation/report binding.

An explicit fake owner ledger decision is fsynced only after its known synthetic
effects are resolved and source retention exists. The resulting effects receipt
pins those actual ledger bytes. No worker empty-list test creates reconciled
facts. Final report acknowledgment likewise pins the actual fsynced fake report
and preserves the owner-allocated original references. Protected disk remains
charged throughout. Actual central enforced hold, summary retention, effects
reconciliation, continuation scheduling and destination launch remain absent.

The fixture's focused vectors are executed in the data-only gate below. They distinguish actual accepted bytes from matching-looking DTOs,
canonical summary byte counts from text counts, original replay from interval
extension, explicit ledger reconciliation from missing/empty inference, and
wrong source/summary/uploader/continuation/expiry refusal. The later actual tmux
and pinned public aq-git composition is a separate pending qualification.


The parent-authorized short Target2 gate on the independent candidate worker0.5
worktree based on bridge040b90f executed:

- `cargo test -p aq-worker --offline --lib continuation::tests`:11 passed,
  178 filtered,0 ignored/failed;0.03s tests after25.01s relocated compilation.
- `cargo clippy -p aq-worker --offline --lib --tests -- -D warnings`:
  passed15.78s; this includes strict checking of the cfg(test) fake owner.
- `cargo check -p aq-worker --offline --all-features --lib`:passed11.59s,
  with the sole expected unused `parking_upload_step` warning. This checks the
  new explicitly test-support-gated opaque source orchestrator's concrete API;
  it is not all-feature strict qualification or end-to-end execution.

The source orchestrator permits only the fixed two Git intent/result pairs and
a final settled observation (at most5 iterations). It snapshots the actual
opaque owner, fsyncs original requests before Git, inspects actual export members
before saving results, and preserves pending IDs on failure. It returns metadata
only and creates no retention, launch or ordinary Suspended result. Its joint
physical execution remains pending. Target2 was released immediately after this
gate. There was no actual tmux/Git/service/coordinator network, provider,
canonical state or destination bootstrap operation.

## Pending actual composition

The next explicitly test-support-gated helper prepares one source-upload outbox
step through the same actual stopped owner, fsyncing a newly pending range
before returning metadata. It returns no raw action bytes/client/send permission
or retention receipt. Begin/Finalize preparation with no changed durable state
avoids a journal rewrite. This helper is authored after317af74 and is covered by the later strict source
gate below; the earlier check result covers the source orchestrator alone.

The actual composition remains pending a reviewed narrow private fixture factory
and a separate physical execution lease. The test must retain real execution,
input, source service and filesystem owners for its full scope. It will use the
root-supplied, read-only SHA-verified public aq-git/host/tmux/static-program pins,
not ambient replacements. Synthetic GitPeer setup remains explicitly separate.
The intended first assertion sequence is actual Running, explicit fake hold and
input-disabled suspension, actual guardian stop, fsync/readback-bound stopped
anchor, compute still charged before settled Git, public checkpoint/export,
compute freed with full protected disk charged, exact durable upload ranges,
fake owner actual-byte/summary retention and explicit ledger/report receipt,
and protected disk still charged with no ordinary Suspended result/new launch.
Lost-response steps preserve the original pending recipe. Unknown stop, missing
source facts, invalid summary/receipt and any changed root/identity remain held.
Different-node bootstrap/ack is WRK16B and is not part of this qualification.


The new metadata-only upload preparation helper passed the separately authorized
`cargo clippy -p aq-worker --offline --all-features --lib -- -D warnings`
in12.17s, with zero warnings. Its use of the real private upload-step seam removes
the previous unused-method warning without expanding API visibility or adding a
client/send/retention path. The unchanged eleven data-only tests were not rerun.
This source gate does not execute the actual opaque-owner upload composition.
Target2 was released after the check; no physical/service/provider operation ran.

## Actual two-case test source (not executed)

The authored `parking_composition_tests.rs` is compiled only through the small
approved native test registration and its physical test is explicitly ignored.
It uses the private actual-owner factory; it never reconstructs execution or
stopped-workspace authority from fixture JSON. The bounded90s test has two
sequential isolated worlds, Supplied and MissingSummary, with separate actual
retained owners/roots. The existing published fixture currently reuses template
wire UUIDs across those disjoint worlds; this is not qualification of two
concurrent cluster attempts or cross-node routing. No admitted IDs or immutable
claim is rewritten. Root approved this isolated-world policy explicitly; it never permits
post-admission identity mutation or a two-node/cluster-uniqueness claim.

The retained static program records its actual workspace cwd. The current test
then seeds tracked and untracked changes as fixture-operator writes while the
real child is Running; these are not claimed to be model/agent-generated edits.
It observes genuine stopped/ECHILD history and a fsync-bound parking anchor,
checks unsettled source still charges compute, runs actual public aq-git
checkpoint/export, and checks compute becomes available while full original
disk remains charged. No synthetic GitPeer substitutes for the public binary.

Transfer preparation fsyncs each pending range before a concrete test-only owner
accepts its bytes. The first chunk reply is deliberately omitted locally; exact
readback plus original request/bytes and cached owner response are required
before acknowledgment. Actual raw export members are SHA-checked and retained
by the fake owner. Supplied then retains canonical summary bytes and records an
explicit fake ledger/report receipt. Missing obtains no checkpoint receipt,
reconciliation or migratable completion, while still releasing proven idle
compute and preserving full disk charge. Execution deadline fields must remain
unchanged. The test introduces no ordinary result/new launch/production owner.

At this source stage no physical case has run. Combined source review and a
separate Root execution authorization remain required, including exact retained
binary pins and source-factory policy choices. Compile-only checks cannot be
reported as actual tmux/Git/retention composition qualification.


### Combined-source gate

Combined source includes the private factory d2f76ce (locally786c714), the
approved four-line test registration and the new ignored two-case test. Root
resolved the choices: keep the existing exact fdbf program pin, seed workspace
edits by the fixture operator with observed actual child cwd, and permit honest
isolated-world reuse of template wire IDs. No replacement executable or admitted
identity mutation was made.

The coordinated short Target2 source-only gate executed:

- `cargo test -p aq-worker --offline --features test-support --lib --no-run`:
  passed16.63s. This compiled but did not execute the physical test.
- `cargo clippy -p aq-worker --offline --all-features --lib --tests -- -D warnings`:
  passed13.35s with zero warnings.
- The one improved data-only owner-binding test passed0.02s after0.16s warm
  compilation,213 filtered and0 skipped/failed. It validates a different but
  internally coherent continuation/checkpoint pair and changed report operation
  before checking owner rejection, avoiding an earlier-only shape rejection.

Target2 was released after these checks. No physical tmux, public Git service,
provider or canonical operation ran. Independent combined-source review and
explicit Root physical execution authorization remain outstanding. The actual
factory uses the existing old-phase synthetic profile's genuine owner flow;
this is not qualification of production refresh launch, central hold/wake,
model-authored edits, different-node hydration or protected-disk release.
