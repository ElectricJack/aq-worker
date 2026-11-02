# Native runtime journal and restart boundary

The native driver uses a separate versioned journal. It does not deserialize a
native offer as the older fixture Offer or share the fixture journal's state
format. The retained Git/source receipts are evidence owned by aq-git; this
journal never becomes a competing authority for Git refs, worktree contents or
central task completion.

## Ownership and bounded durable records

One exclusively locked private root owns one enrolled node's runtime journal.
Persist the new logical runtime boot and actual kernel boot separately. Every
attempt retains its original node/boot, exact NativeOffer, local reservation,
qualified runtime/adapter/renderer references and monotonic deadlines. Capacity
is derived from all held attempts, including unknown launches and unreleased
results. No uncertain attempt frees a slot or disk reservation.

Use typed, bounded operation records for acceptance, artifact/material fetch,
preparation, host launch/stop, source transfer and result submission. Before I/O,
commit the stable operation ID and exact canonical request bytes/digest, including
original owner and journal revision. A retry transmits those bytes unchanged;
a fresh delivery nonce and actual send/receive interval are separate metadata.
Same ID with changed bytes is a local conflict. Receipt recording is another
fsynced journal transaction; a failed/lost response leaves the original intent
pending for the owning endpoint's exact replay or reconciliation.

Atomic replacement requires temporary file fsync, no unsafe path following,
rename and directory fsync. Failure after a possibly effective write poisons the
current in-memory writer until the durable record is reloaded and validated.
A corrupt, missing or newer unsupported journal is not an empty node. Bound
records, retained operation counts, encoded snapshot size and pending bytes;
reject capacity before accepting another offer. Cleanup requires actual release
receipts and preserves any operation identity whose replay is still possible.

## Preparation and execution transactions

Persist an offer's conservative local expiry and a no-launch reservation before
Accept. Keep original preparation/grant receipts and their sampled delivery
clocks separate. The private material owner returns a digest-bound receipt only
after its filesystem durability boundary; retain that exact private-binding pin
before Prepare. Reopening material later must compare the pin, not invent a new
receipt from a self-consistent file. Final startup bytes and actual hydrated Git
workspace likewise require their own verified owner receipts.

Persist an exact LaunchIntent before asking the tmux host for any physical side
effect. After that point, missing replies are LaunchUnknown and may never cause
another launch with a replacement ID. Only the actual qualified host's original
physical identity and the baseline/exec requirements can produce Started. Persist
the actual Started request before its network send. A network ACK is not another
physical start; a host callback is not a central Started receipt.

Renewals preserve the original hard deadline. A newer revision can extend only a
still-live permit; same-revision retries keep the previous local expiry. Retain
accepted delivery evidence before updating the independent guardian permit. The
safe failure order may shorten execution but cannot extend unrecorded authority.
The guardian remains responsible for expiry while the driver, storage, network
or model is blocked. Heartbeat success never substitutes for renewal.

Cancellation and expiry persist stop intent, request the existing guardian stop,
and record actual physical stopped evidence. Unknown or failed stop leaves holds
in place. No writer-dependent checkpoint/export, workspace reuse, cleanup or
replacement follows uncertainty. After actual Stopped admission, source/effect
owners and the immutable result ledger run their separate replayable operations.
Recorded results retain resources until explicit settlement/release; they do not
cause local task completion or a graph compare-and-swap.

## Runtime restart and boot mismatch

The initial native profile is stop-and-preserve, not live reattachment. A new
runtime process gets a new logical boot ID even on the same kernel boot. The old
journal remains evidence for its old session identities. It cannot be relabeled
with the new boot, supplied to a new-boot Reconcile request, or used to renew an
old grant as if the runtime had never restarted. CLOCK_BOOTTIME values are never
compared across different kernel boots.

Current daemon registration refuses a changed boot while prior task claims are
held. Recovery therefore precedes new registration: inspect the exact original
host owner, stop/preserve surviving writers, and submit only the appropriate
historical owner reports with the original binding/runtime qualification under
the node's current credential. Await actual settlement and release before new
registration and offer polling. Recovery cannot renew old execution or restore
agent credentials. Missing physical owner evidence remains unknown.

The daemon authenticates the enrolled machine and recorded admitted boot; it
cannot independently observe a worker process restart. Refusing live old-boot
reuse is a qualified driver invariant. Registration refusal alone is not a
server quarantine against a malicious worker that keeps presenting old boot
metadata. A future recovery/quarantine fence must be explicit if required.

A granted launch without centrally admitted Started is not NeverGranted. The
current post-Started stop route cannot close that gap. A separately specified,
versioned no-start/uncertain-launch recovery owner is required before claiming
complete restart recovery. Until then, these attempts remain held and visible;
no fallback boolean, forged Started or altered frozen protocol will release them.
A future live-reattachment profile needs explicit cross-boot admission and proven
surviving process ownership, independently versioned and qualified.

## Implementation sequence and acceptance

1. Implement the private journal storage/validation and capacity accounting, with
   interruption tests at each file durability boundary and immutable operation
   replay. Start with reservation and preparation transitions.
2. Connect exact0.1.7 HTTP observations, bounded downloads, the private material
   owner and actual aq-git workspace hydration. Persist all original local clocks.
3. Implement the host durability callbacks and outgoing Started/Stopped records;
   qualify lost launch/stop ACKs with actual tmux processes and independent expiry.
4. Connect source/result/release owners and implement bootstrap stop-and-preserve.
   Complete the separate no-start recovery contract before closing restart gates.

Core acceptance includes full disk, corrupt/missing journal, new logical/kernel
boot, changed operation bytes, late grant/renewal, blocked storage/network,
unknown physical launch/stop, revoked credentials, lost source/report ACKs and
capacity exhaustion. Synthetic clocks and peers qualify bounded local behavior;
the product gate still needs an actual daemon, Git owner, worker and selected
harness on one real task. This document is an implementation plan, not evidence
that those journal transitions are installed.

## Prior qualified format1 storage slice

At worker `281f8a4`, `native::journal` provides explicit initialization, exclusive private storage,
no-launch reservation, stable acceptance and observed preparation persistence.
It is a separately versioned format1; fixture journal formats are unchanged.
The first slice holds at most64 attempts and8MiB of encoded state, with configured
slot/memory/CPU/disk reservation limits. Every retained attempt remains charged,
even after expiry or an unknown network outcome. These are logical budgets, not
proof of physical disk headroom or sustained storage latency.

Initialization requires a newly created root. Reopening requires an intact,
checksummed supported snapshot with exact node/attempt/request associations.
The root and members are owner-only and opened relative to retained directory
descriptors without following symlinks. Atomic mutation writes one exclusive
pending file, fsyncs it, renames and fsyncs the directory. Any possibly effective
write failure poisons the writer. A published snapshot survives; an unacknowledged
pre-rename pending file is discarded only after validating the published snapshot
and pending file safety. Missing state never triggers automatic initialization.

Reservation identity, original deadline, exact offer, original installed runtime identity/qualification and
resource holds precede acceptance. Acceptance retains one stable request and its
canonical digest before a caller may send it. Retries preserve the original
request and remaining-time claim; a fresh network nonce is separate. The native
HTTP observation exposes its actual send/receive CLOCK_BOOTTIME samples, which
are retained with the preparation receipt and its non-extending local deadline.
No credentials or private material bodies enter the journal.

Every open after process restart is inspection/stop-and-preserve only, even on
the same kernel boot. It cannot advertise free capacity, reaccept, renew or
reconstruct a live preparation/grant wrapper. Original identities and kernel
clock provenance remain history. This initial conservative profile also requires
explicit recovery/reinitialization for an empty reopened journal; it provides no
automatic release/reset API. Callers must run its blocking fsync work on their
bounded storage lane before any external effect.

Remaining integration is explicit: durable fetch/material/Prepare intents, actual
Git hydration, launch/stop callbacks, guardian permits, result/release ownership,
bootstrap recovery and the no-start recovery protocol. No runtime capability is
advertised by the mere presence of this storage module. Physical ENOSPC, power-loss
filesystem durability and real process restart recovery need additional system
qualification; injected durability-boundary failures are component evidence only.


## Current input-operation candidate

The separately qualified [durable input slice](WRK-08-native-preparation-slice.md)
advances native journal storage to format2, refuses old format1 unchanged, and
retains exact original artifact/material operations plus independent installed
material pins. Inline artifact bytes remain RAM-only; durable receipt metadata
never reconstructs bytes or a live permit. Requests are batched before network
I/O, and material pins follow the filesystem owner's durability boundary.
Reopened state remains stop-and-preserve. There is still no Prepare gate, Git
hydration proof, launch callback wiring or runnable registration.

The later native launcher must record its intent before invoking host/adapter
startup hooks, including adapter.prepare; the existing later callback alone is
not proof that no physical preparation effect occurred. See the daemon's
[granted-launch closure plan](../../../aq-server/docs/implementation/AQ-08-granted-launch-closure-plan.md)
for the still-open granted-without-Started recovery boundary.

## Bounded native source owner checkpoint

The follow-up [hydration slice](WRK-08-native-hydration-slice.md) adds format-3
source range and finite Git intents, actual fresh retention/workspace proof and
private staged files. It preserves stop-and-preserve on reopen and refuses old
formats unchanged. Its conservative default Git disk charge is 21 GiB + 1 MiB;
production Poll/Reserve must supply a compatible qualified capacity envelope.
Prepare/launch and physical disk-headroom closure remain pending.

The subsequent [Prepare input join](WRK-08-native-prepare-join.md) adds an immutable
outbound Prepare intent in format4. Formats1/2/3 are refused unchanged; their
previous qualification is historical, not a migration claim. A durable intent
still supplies no grant, process permit or permission to resume after reopening.
