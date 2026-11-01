# WRK16A: actual stopped workspace and capacity bridge

Status: source proposal only, based on worker `69abe91`. No bridge runtime,
journal schema, process, Git or build has been changed by this document.
Work owns `native/continuation/**` orchestration/history/source operations;
Human owns all native journal schema integration and the final FORMAT choice.
This lane owns the narrow actual physical/fsync/workspace bridge after review.

## Retained source of authority

`DurablePhaseExecution` already retains the actual observed execution receipt,
original `InputPipeline` owner, retained input map and actual prepared inputs.
Its private `launch_scope` checks process-local ownership association before
constructing the physical scope; `scope.check` separately checks current launch
authority. Stop and retention must use the former association without running
the latter after execution expiry. A stale grant may not run an agent, but
does not prevent stopping its already owned writers or preserving source.

Use that actual owner and the retained `HydratedWorkspace`. Never construct a
source owner from a `Workspace`, attempt ID, `NativeStopped`, CON03
`ReportedStopped`, terminal receipt or JSON grant. The original immutable offer
derives the exact lifecycle binding through
`aq_admission::transport::lifecycle_binding`; no caller-supplied fake task,
lineage or generation can replace it.

The first entrypoint is test-support-only:

```rust
impl InputPipeline {
    // CON03 records and fake hold are explicitly synthetic, not authenticated
    // production delivery. Actual execution/input/pipeline owners are required.
    pub async fn begin_synthetic_parking(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        intent: aq_lifecycle_protocol::SuspensionIntent,
        claim: aq_lifecycle_protocol::CheckpointClaim,
        fake_hold: aq_session_protocol::ArtifactRef,
    ) -> Result<DurableParkingFence>;

    pub async fn stop_parking(
        self: &Arc<Self>,
        fence: DurableParkingFence,
    ) -> Result<DurableStoppedWorkspace>;
}

pub struct DurableParkingFence { /* noncloneable private actual owners + fsync */ }
pub struct DurableStoppedWorkspace { /* noncloneable private actual owners */ }
```

Neither type derives Deserialize/Serialize, contains a public constructor, or
implements a public authority trait. Metadata getters alone grant no Git,
capacity or execution effect. Production hold/routes remain unsupported.
No ordinary execution result is converted to Suspended; no successor bootstrap
or16B behavior is introduced.

## Exact intent, roots and input fence

The existing actual retained scope supplies attempt, execution receipt digest,
immutable offer/profile family, launch binding and source workspace. The
bridge verifies exact current journal association and selects its existing
registry entry; it does not take arbitrary host paths or workspace IDs.

Before intent fsync, capture checked canonical directory FDs for the original
private aq-git managed root and exact actual workspace route. Persist their
device/inode identities with the retained source binding, workspace ID and
generation, paths and immutable host-intent digest. Hold those FDs in the
opaque fence/token. A renamed/replaced route, changed source repository/binding,
foreign execution owner, missing host intent or changed source preparation
refuses the source bridge. A serialized root pin is not an opened owner.

Work's concrete `ParkingIntent` also stores exact CON03 intent/checkpoint and
fake hold, stable checkpoint/export/transfer/begin/finalize/report operation
IDs, before their first effects. The constructor checks exact lifecycle and
continuation/wait equality, summary budget and original timing bounds. Sampling
these synthetic claims cannot extend the execution or host hard deadline.

Fsync the exact intent through the existing one-job bounded storage owner.
Only then synchronously mark the registry stop fence so new launch/renew/local
ordinary effects cannot be admitted. Cancellation of a caller must not split
the committed intent and process-local fence; perform that fence as part of
the already admitted storage handoff. Transport cancellation is a safety
action, never rollback of an earlier upstream effect.

Registry fencing alone does not prove central mutation revocation or a terminal
controller FenceAck. The first synthetic profile must either have terminal
input disabled or qualify the existing actual controller=None fence separately.
Never emit a fabricated fence ACK. Production central hold and ordinary-agent
mutation closure remain explicit missing owners.

## Actual stop followed by durable proof

Use existing exact `SessionHost::recover`/private `SessionHandle`, tied to the
retained native host intent and root identity. Request safety stop, wait for
its owned guardian, and obtain the non-deserializable `StoppedProof`. Validate
guardian nonce/incarnation, full original host intent, machine/PID namespace,
launch/session/configuration, actual Stopped phase, positive reap count and
kernel ECHILD. EOF, idle output, turn completion, Drop or a past report do not
substitute for this proof.

The actual proof remains in the original registry's private physical entry.
Capture only a process-local clone of that opaque proof for the admitted
storage job. Persist existing native stopped history and Work's exact parking
stop anchor referencing its proof digest and stopped revision. Bind it to the
already committed parking intent and record its own revision. These can be
separate committed steps; failure/cancellation between them retains resources
and produces no stopped-workspace token or compute-release fact.

Only after the final fsync and readback checks return
`DurableStoppedWorkspace`. Its private fields retain:

- original pipeline/retained-map association and actual execution input scope;
- original registry entry and opaque `StoppedProof`;
- actual workspace/Git owner and retained root/workspace FDs;
- exact lifecycle/launch/profile/execution/suspension/source association;
- committed parking intent digest, stop-anchor digest and record revisions.

No journal reopen can reconstruct this token. Retrying with the original live
owners may match the same intent/proof; restart requires independent physical
and source reobservation, not a JSON-to-live conversion. First16A need not
claim that recovery join is complete.

## Dirty stopped inspection and narrow source seam

Current `HydratedWorkspace::revalidate_execution` requires an unexpired deadline
and an exact clean base. It is unsuitable after real dirty agent work and must
not be weakened for launch. Add a separate private stopped-source inspection
that requires the actual stopped token. It validates original source/import/
create association, actual retained roots, current aq-git repository/binding,
workspace ID/generation/path/allocating operation, while allowing supported
dirty/untracked contents. Current head is observed for the stable checkpoint
request; a clean-source assertion is not invented.

Work's checkpoint/export APIs accept `&DurableStoppedWorkspace` and operate
through the original bounded storage/Git owner. No caller-provided Git socket,
workspace route, new operation ID or DTO proof may override its captured source
identity. Persist each original Git mutation UUID/request before exchange and
match/replay exactly after loss. Do not reuse the legacy runtime's distinct
guardian/stopped-proof DTO or ordinary-result settlement path.

## One compute/disk projection, coordinated schema

Native `State::validate` and `Journal::available` currently duplicate full
attempt resource charging. Human must integrate **one** shared checked charge
projection with Work's concrete history. The projection is history/accounting,
not a live stopped-token constructor.

Release slot/CPU/memory only when exact parking fence, actual native stopped
anchor and source association validate and **both required checkpoint and
export operations** have recorded independently validated successful results.
No empty operation map, unknown stop or Git result is settled success. Unknown
stop/Git, unsupported source or missing prerequisite retains all compute and
disk. No upload or retention ACK is needed merely to free proven idle compute.

Disk stays charged independently for original workspace, private retained
inputs, pending transfer/export and shared Git pool. The existing native
reservation already covers workspace + full artifact + pool, so first16A may
retain the full reservation unchanged and reject/drain any observed excess;
never reduce it because compute was freed. Local retirement and disk release
require later exact central/aq-git owners and are not implemented incidentally.
The64-retained-attempt/8MiB journal bound remains visible.

No lane adds a sidecar journal or modifies FORMAT independently. If Human's
concrete format11 freezes before these fields, Root must choose explicit12.
Old roots remain refused/preserved; no migration invents stopped authority.

## Bounded implementation and meaningful checks after approval

1. Agree Work's concrete ParkingHistory/stop anchor/root pins and Human's exact
   schema/validated mutation hooks. Add only sibling physical source plus tiny
   private launch/input-scope accessors; no changes in another lane's worktree.
2. Service-free checks reject foreign actual owner, bad binding/profile/source,
   absent root identity, changed immutable intent/proof/record and fsync cuts.
   Assert no token/capacity release on failures, and no serde construction.
3. Only after source review, an explicit disposable fixture may launch the
   pinned local tmux synthetic child and public aq-git. Prove actual writes,
   stop/ECHILD then checkpoint/export; hold upload/retention while showing free
   compute and charged disk. No provider or canonical checkout is used.

Production suspension delivery/hold, central receipts/effects, terminal-input
fencing if enabled, restart reobservation, retirement and fresh-node bootstrap
remain separate gates. No suspend/hibernate/wake product acceptance is claimed.
