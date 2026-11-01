# WRK16 worker suspension and continuation: audit and first implementation proposal

Status: proposal for root review, November1,2026. Worker baseline69abe91.
No suspension runtime, journal format change or new coordinator route is
implemented by this document. WRK16 is beginning, not fully qualified hibernation.
No SQL, canonical data, live agent, provider, tmux process or build was used in
this audit. The existing qualified source/host foundations are retained.

## Product boundary

Implement the worker half of:

`accepted checkpoint claim → fenced ordinary activity → proven stopped writers
→ final source checkpoint/export → centrally retained artifacts/effects
→ separately released compute and protected disk → fresh successor bootstrap/ack`.

AQ owns wait registration/hold, dependency verification, immutable outcome,
wake scheduling, winning successor and budgets. A worker does not evaluate graph
predicates, turn a hint into success, keep a lease merely to remember a sleeper,
or silently resume the predecessor. Early dependency resolution is independent
of physical parking and never bypasses an uncertain old writer.

Read alongside agent-q `docs/architecture/0004-session-operations-waits-and-
notifications.md`§4, `docs/plans/0001-v2-milestones.md` WRK16/AQ25/AQ26/INT03,
and frozen CON03. Existing runtime acceptance is evidence for mechanisms, not
proof that these mechanisms have already been joined for suspension.

## Audit of actual reusable code

| Foundation and source | Existing behavior | Missing worker join |
| --- | --- | --- |
| `aq-lifecycle-protocol/src/continuation.rs` | Exact Binding, SuspensionIntent, CheckpointClaim, ParkingReport, retention/effects claims, ContinuationAttachment and BootstrapAck; separate suspension disposition | Pure validation is not authenticated delivery, stop, retention verification, durable runtime state or permission gating |
| `aq-admission::transport::lifecycle_binding(ProfiledOffer)` | Derives exact project/task/lineage/session/attempt generation from an actual immutable offer | Reuse it; agent-access JSON Binding lacks lineage and cannot replace it |
| `native/preparation/execution.rs` | Non-deserializable DurablePhaseExecution requires actual received phase authority and fsync; expired records are history only | Add a profile-neutral dispatch seam through Runner's owner; do not coerce Refresh/legacy JSON into this type |
| `native/launch/mod.rs` | Registry can synchronously fence a host, cancel transport, physically stop through the guarded tmux host, obtain private StoppedProof, then persist history | No opaque fsync-bound stopped-workspace token; registry fence alone is not central agent-mutation revocation or a terminal-controller fence |
| `aq-session-host::{SessionHandle,StoppedProof}`, `terminal_io.rs` | Exact guardian/incarnation ECHILD proof; typed terminal Fence/FenceAck; recovery stops and preserves, never relaunches | Pair suspension identity and ordinary-input closure with these existing mechanisms; no summary/EOF/Drop substitute |
| `native/journal/source.rs`, `native/hydration.rs` | Native Base discovery/chunks, verified Git manifest, import and workspace creation under existing input provenance | No native checkpoint/export/upload/retention/retirement history |
| `source.rs`, `source_transfer/{upload,download,files}.rs`, `transfer_client.rs` | Legacy runtime has stopped-proof-gated aq-git checkpoint/export, immutable transfer UUID/chunks, same-ID replay, exact central source receipt and release/eviction rules | Extract/reuse pure operations and validation; legacy Runtime requires its own guardian/protocol types and ordinary result settlement, so cannot simply wrap or forge its records |
| `journal.rs::protected_usage` | Legacy compute can release after confirmed stop while protected source/pending storage stays charged; unknown source operations conservatively hold resources | Native `Journal::available` currently charges every retained attempt all slot/CPU/memory/disk; needs one coordinated native capacity projection |
| `aq-agent-client::{Allocator,NodeClient,StartupContext}` | Fresh session credential provisioning, exact bounded context, original operation lookup/retry without renewing agent writes | No continuation-specific bootstrap mode/ack command or coordinator enforcement; do not reinterpret ordinary client recovery as continuation acknowledgment |
| `native/journal/phase_terminal.rs` | Stable Started/Stopped outboxes and exact receipts anchored to actual phase/host history | Suspend must remain a separate parking disposition and never call ordinary result success/failure settlement |

The current legacy `ProcessOutcome` is only Success/Failed/Cancelled/Expired/
Unknown, and DirectiveKind only Cancel/Drain. Neither becomes Suspended through
an undocumented enum reinterpretation. Use CON03's separate parking domain.

## Exact contract gaps to freeze with owners

1. **Wait/hold delivery.** CON03 SuspensionIntent is an unauthenticated wire
   claim. AQ25's initial internal wait ledger cannot claim a real scheduler hold
   until its AQ08 admission hook exists. Before a production worker accepts
   suspension, require an authenticated machine-delivered intent, exact current
   offer/execution binding, continuation/wait generation and actual central hold
   receipt. No production route/capability is advertised by this proposal.
2. **Claim versus retention.** AQ lifecycle `CheckpointRecorded` acknowledges a
   claim digest; it is not a RetentionReceipt. Existing resumable source protocol
   supports Base/Checkpoint Git transfers, not a general summary artifact store.
   AQ09/AQ26 must issue an actual retained-summary receipt (and source receipt
   mapping) under the predecessor Binding. No worker-authored receipt or upload
   EOF becomes central retention.
3. **Effect reconciliation.** Pending operation IDs are claims. A complete,
   current source-owner reconciliation receipt is required; an empty local list,
   bounded page or unavailable credential cannot mean Reconciled.
4. **Successor source.** Native hydration requires Base with checkpoint==base,
   while predecessor checkpoint export can have original base!=checkpoint.
   Central AQ09/aq-git must produce a fresh successor-bound Base export rooted
   at the retained checkpoint. Do not relabel/reuse the predecessor transfer
   manifest, reinterpret an old artifact as fresh authority or rewrite bundle
   metadata without its owner.
5. **Bootstrap permission.** An attachment is not a grant. AQ26 must join a fresh
   offer/material/source admission with the exact attachment and enforce scoped
   read/ack/recovery-only behavior until authenticated acknowledgment. Rendering
   the attachment in instructions cannot enable ordinary mutations.
6. **Worker journal ownership.** Baseline native format10 has no continuation
   records. Runner owns new format11/profile-family adoption; its layout is not
   frozen. WRK16 must request a nested record/validator projection through that
   owner, never add a sidecar journal or independently change FORMAT. Old roots
   remain refused/preserved; no automatic migration reconstructs live authority.
7. **Protocol import only.** The already-qualified aq-lifecycle-protocol0.1.0 is
   in this workspace and already transitively consumed by aq-admission, but
   aq-worker does not directly import it. Direct typed CON03 use needs an exact
   edge to that existing package or an owner-approved existing re-export. This
   is a review decision under the no-new-dependencies constraint; no package,
   version, registry dependency or duplicated contract structs are added here.

8. **Bounded local history.** Native roots currently cap retained attempts at64
   and have no suspension-specific retirement path. Slot release alone cannot
   promise unlimited subsequent admissions. Add an explicit receipt-bound local
   retirement/archive decision through the same journal owner after source and
   required history are centrally retained/released; never timed-GC an active
   continuation. The first slice must report this metadata bound honestly.

These are ownership joins, not reasons to invent another workflow engine.
The first fake coordinator can demonstrate the worker mechanics while each
missing real central authority remains explicitly unqualified.

## Proposed narrow typed seam

Names below are proposed interfaces, not new serialized authority formats.
Constructors are private to their current authenticated/physical/fsync owner;
none derives Deserialize, Clone/Debug containing secrets, or a test-only
DTO-to-live constructor. Frozen CON03 objects remain ordinary history/claims.

```rust
// Profile-neutral wrapper selected inside Runner's private family dispatch.
// First adapter may wrap actual DurablePhaseExecution; Refresh must use its
// own actual receipt/proof adapter after Runner qualifies it.
struct SuspensionExecution { /* actual retained execution/profile owners */ }
struct ReceivedSuspension { /* authenticated delivery + sampled original limit */ }
struct DurableSuspensionFence { /* exact durable intent + held authority */ }
struct DurableStoppedAttempt { /* actual StoppedProof + synced record owner */ }
struct RetainedParking { /* actual verified central receipts, not wire claims */ }
struct AdmittedContinuation { /* fresh execution + verified attachment owner */ }
struct DurableBootstrapAck { /* exact authenticated ack receipt + synced record */ }

impl InputPipeline {
    async fn fence_suspension(
        &self, execution: &SuspensionExecution, received: ReceivedSuspension,
        checkpoint: Checkpoint,
    ) -> Result<DurableSuspensionFence>;
    async fn stop_suspension(
        &self, fence: DurableSuspensionFence,
    ) -> Result<DurableStoppedAttempt>;
    async fn checkpoint_suspension(
        &self, stopped: &DurableStoppedAttempt, git: &GitClient,
    ) -> Result<ParkingProgress>;
    async fn advance_parking(
        &self, stopped: &DurableStoppedAttempt,
    ) -> Result<ParkingProgress>; // one bounded same-ID transfer/receipt step
    async fn prepare_continuation(
        &self, fresh: SuspensionExecution, admitted: AdmittedContinuation,
    ) -> Result<BootstrapOnly>; // no ordinary write authority
    async fn record_bootstrap_ack(
        &self, bootstrap: &BootstrapOnly, observed: AuthenticatedBootstrapAck,
    ) -> Result<DurableBootstrapAck>;
}
```

`DurableStoppedAttempt` binds the admitted protocol/profile family and original
execution-receipt digest; full lifecycle/launch binding; actual guardian,
launch and process incarnations; exact ECHILD observation; source workspace/
repository/binding generation; and fsynced journal record digest/revision. It
cannot be constructed from attempt ID, HostObservation JSON, reaper/Drop result,
phase-terminal receipt or a CON03 ReportedStopped alone. Reobservation may use
existing stop-and-preserve recovery and actual StoppedProof; never reconstruct
an executable permit from disk.

Host/common mechanisms remain in aq-session-host so central sessions can reuse
actual stop/input fencing without importing worker scheduling/private state.
Worker-specific source/capacity joins stay in aq-worker. New public authority
traits that arbitrary callers can implement would weaken this seam; avoid them.
The fake coordinator is a configured finite test peer using normal typed
transport verification, not a privileged production or live-authority bypass.

## Durable facts and ordering

Request one nested ContinuationHistory from the native journal owner. It stores
canonical CON03 records and immutable operation identities, not a second mutable
state-machine database. Schema/field layout and bounded history reserve must be
approved by Runner before mutation. Derive public disposition from these facts:

1. Journal exact authenticated suspension intent, central hold, predecessor and
   continuation generation, summary claim/digest or explicit Missing claim,
   stable input-fence/stop/Git/upload/report IDs before their first effect.
2. After checkpoint acceptance, close new ordinary AQ mutations centrally and
   apply the existing exact terminal controller fence (controller=None) where
   input is enabled. Preserve/reconcile earlier admitted operations. Stop remains
   a safety action even if the agent credential/old grant has expired; no worker
   grant extension is minted. Preserve narrow node status/recovery access.
3. Synchronously fence native launch/renew/input dispatch, cancel owned serving
   transport, and stop the actual tmux guardian/writers. Persist exact opaque
   stopped proof before any source checkpoint or compute-release fact. Unknown
   stop holds compute+disk and cannot produce StoppedRetaining.
4. With that proof, use public aq-git operations for final supported dirty and
   untracked source. Persist original Git UUID/request before I/O; re-query/
   replay same operation after lost replies. Unsupported source, oversize data
   or uncertain aq-git disposition remains held/protected, never empty success.
5. Export immutable source, reserve original transfer identities and bounded
   chunk descriptors before send, then advance existing source-transfer steps.
   Send the same report/receipt request after loss, not a new operation. Upload
   progress is not retention. Keep control/safety lanes independent of bulk.
6. Persist verified source+summary central receipts (exact artifact/digest/role,
   not merely one receipt of each role), authoritative effects
   reconciliation and parking report acknowledgment independently. Only these
   prerequisites yield RetainedSuspended/migratable. Report delivery failure or
   early outcome cannot clear retained source or relaunch the predecessor.
   The first slice requires a supplied bounded summary for automatic migration;
   an explicit Missing claim enters held recovery, not fabricated context or
   automatic RetainedSuspended.
7. Compute release projects only from the committed suspension fence and actual
   stopped proof. Disk remains charged for workspace, pending bytes, Git handles
   and shared repository pool until exact explicit central release and aq-git
   retirement/conditional eviction complete. Do not weaken legacy conservative
   unknown-Git accounting in this increment. First positive proof can release
   compute after known checkpoint/export while upload is deliberately pending.
8. A retry never resumes the old process. A different eligible node receives a
   normal fresh offer/attempt/session, fresh material/agent credentials and a new
   base export. Bounded startup context adds exact wait/outcome, summary/source
   references, pending effects and held task-mail references under existing
   context limits; required context is never silently truncated. Native provider
   state/old credentials/raw private folders are not copied.
9. Successor stays bootstrap-only until exact generation/admission/attachment/
   outcome acknowledgment is authenticated and fsynced. Same-ID same-bytes replay
   returns the original receipt; wrong generation/outcome/actor conflicts. Failed
   or lost ack never authorizes ordinary mutations or a second winning attempt.

Deadlines retain their clock domains. The accepted delivery defines the bounded
checkpoint-cooperation window; the old execution grant/hard BOOTTIME limits are
never extended. After physical stop, bounded node retention/recovery work uses
its own authenticated existing authority, not expired agent writes. Disk and
private-material cleanup require their own verified owners/proofs.

## Bounded implementation sequence for approval

**16A: local parking and exact retention.** Integrate the coordinated nested
history and capacity projection, typed fence/stopped seam, public aq-git final
checkpoint/export and reusable transfer operations. One actual pinned local tmux
synthetic child edits a real disposable worktree. A finite fake coordinator
accepts the frozen suspension, source/summary receipt and effects joins. Prove
compute release with protected disk while receipt delivery is held, then
RetainedSuspended only after verified receipt/reconciliation. No provider and
no ordinary result settlement. Real central wait/hold/retention routes remain
gated where their production owners do not yet exist.

**16B: fresh-node bootstrap/ack.** Use two distinct node/boot identities and fresh
journal/material/source roots. Central test owner makes the fresh base export
from retained checkpoint and a new eligible offer; ordinary phase preparation
mints actual opaque fresh authority. Restore without nodeA's directory, native
provider history or credentials. Exercise bounded bootstrap-only APIs and
same-generation ack replay, then enable ordinary actions only through the
coordinator's verified acknowledgment gate. This qualifies a synthetic worker
mechanism, not real AQ26 scheduling or INT03 product acceptance.

Do not call WRK16 complete after16A. No implementation starts until root reviews
this proposal and coordinates the journal/central receipt interfaces.

## Proposed files and ownership

| File/change | Ownership |
| --- | --- |
| `native/continuation/{mod,suspend,retention,bootstrap}.rs` | WRK16 bounded orchestration and opaque owner types; no second persistent state store |
| `native/journal/continuation.rs` plus Attempt/validator/capacity integration | Runner coordinated format11 mutation; final names/layout pending owner approval |
| `native/launch/mod.rs` narrow stopped-token/input-fence exposure | Shared with Runner; preserve actual private StoppedProof and family dispatch |
| `source.rs`, `source_transfer/{upload,files}.rs` pure extraction | WRK16 reuses existing validators/requests/chunk reads; legacy Runtime behavior and qualified cases retained |
| `native/http/continuation.rs` | Only after AQ25/26 freezes authenticated machine delivery/lookup/receipt surfaces; no fake mounted production capability |
| Existing `aq-agent-client` context/provision adapter | Bounded continuation context/fresh credentials; bootstrap permission/ack remains authoritative AQ-owned join |
| `tests/continuation.rs`, finite fixture and crash/replay cases | Fake coordinator + actual pinned tmux and public aq-git, opt-in physical fixture; ordinary pure tests service-free |

Do not modify native journal root/FORMAT, publish a crate, add an ordinary
playbook command, copy transport/auth engines, or enable production capabilities
as incidental work. Use existing dependencies; the exact already-present CON03
import edge is the only dependency decision called out for root review.

## Minimum meaningful acceptance and failure matrix

| Boundary | Required proof |
| --- | --- |
| Suspension/summary acceptance | Stable operation bytes, exact Binding/wait/continuation; missing summary typed; early outcome recorded without bypassing parking |
| Input fence/stop intent | Fsync failure cannot authorize checkpoint/release; stale ordinary input denied; existing admitted effects retained |
| Stop reply lost/crash | Exact guardian recovery/actual ECHILD or Unknown; no launch replay, no compute/disk release from EOF/turn-end/summary/Drop |
| aq-git checkpoint/export reply lost | Same UUID/request after reopen; unsupported/missing source and changed object refuse migratable parking |
| Chunk upload/final receipt loss | Exact offsets/bytes/IDs and replay; no second transfer; pending source remains charged; compute may independently be free |
| Central retention/effects unavailable | StoppedRetaining/Held only; no fake empty reconciled set, no protected disk release |
| Release intent/cleanup crash | Receipt-bound explicit decision, same retirement/eviction operation; unknown storage retained/draining |
| NodeA unavailable/nodeB bootstrap | New node/boot/attempt/session/credentials; freshly re-exported Base pins checkpoint; no original directory/private session dependency |
| Agent credential expired | Old agent mutations refused; bounded node safety/recovery still stops and retains; no renewed old execution grant |
| Bootstrap ack lost/duplicate/stale | Same admission/attachment/outcome/generation replays receipt; stale/foreign ack cannot lift hold; no duplicate successor |
| Worker restart at each stage | Journal facts reopen as history; reacquire only independently verified safe owners; no serialized live-token construction |
| Capacity | Proven stopped compute versus still protected disk shown separately; bounded64 attempts/8MiB journal and existing transfer/disk limits honored |

Record source pins, fixture binaries and measured focused test durations only
after implementation. This source audit has run no runtime tests and claims no
new suspend/hibernate/wake behavior is already operational.
