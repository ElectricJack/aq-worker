# Initial-grant scoped material-presence producer

This component explicitly consumes exact aq-admission0.1.9 archive
`30f4cfca11ff6840eab325026dfea33dd55c23e25099dedb303b52a1ede75b49`.
Legacy0.1.7 source/archive stays unchanged. Dependency adoption does not activate
accepted-preparation v2, a host, provider, renewal driver or runtime scheduler.

## API and scope

A trusted `QualifiedPresenceProfile` constructor verifies actual raw
qualification body bytes against the pinned Definition reference and configured
adapter/evidence references. Formatted Definition JSON is accepted up to the
server owner’s8192-byte limit; the raw hash, not a reserialization, pins it. Every inventory/report must match original admitted
runtime and runtime qualification. A DTO/capability string is not qualification.
`InputPipeline::scoped_presence` creates a bounded process owner from the current
original journal and trusted installed harness qualification references. It takes
the driver's actual last node/boot inventory sequence, sharing the legacy counter
(last1→firstv2sequence2), not a reset or an inferred snapshot revision.

Inventory operations use only empty `provisionable_only`. This makes no worker
credential installation or global availability assertion; the daemon derives
actually authorized provisionable bindings. An opaque actual `InventoryReceipt`
acknowledgement is required for a scoped report, including its normalized
snapshot digest. A decoded receipt alone cannot construct that acknowledgement.

The first producer supports exactly one original material lease and one opaque
`DurableInitialGrant`. It refuses zero/multiple-material grants and any v2 or
renewal authority it does not own. The anchor names the actual immutable initial
Grant operation/revision/absolute expiry, not a reconstructed RawGranted object.

## Actual read and timing

Report observation starts by checking the exact opaque token, pipeline owner,
current journal record/kernel namespace and original local cap. That check does
not assume the target material is Present: Missing/Unknown must be observable.
Only then reopen the owned lease directory, actual canonical installation record
and actual material using bounded descriptor-relative nofollow, regular-file,
single-link,0600/0700 and current-owner checks. Hash actual private bytes; never
substitute the cached historical material digest. Secret bytes are zeroized and
neither logged nor serialized. The exact typed installation record/hash must
match the original pinned receipt and scope.

Recheck owner/scope/kernel and file identities after I/O. Positive requires live
original permit after the read and before publication. A read begun under a live
permit may yield scoped Missing/Unknown after its expiry without execution
permission; no expired retrieval lease is reused as execution authority. A
readable positive whose permit expired is refused, not mislabeled unreadable.
Read/send interval is at most the frozen2second bound. The producer stamps
`sent_ns` once immediately before original publication; exact retry preserves
body, operation ID, sequence and timestamps. No wall-clock synchronization or
renewed TTL is inferred from delivery/retry.

## Bounded operation ownership

Allocate node/per-attempt highwaters before publication or cancellation. Retain
one current inventory operation and at most one current operation per admitted
attempt (maximum64), with frozen64KiB request/4KiB receipt bounds. An unresolved
publication is retained for explicit identical retry, never silently replaced.
Acknowledged or definitely unsubmitted failures may be superseded with a new
higher sequence. Actual reads happen once per operation; cancellation of an
admitted storage job does not cancel it or implicitly publish its result.
A process-local highwater cannot be reconstituted as positive authority on
restart: reopened journals cannot produce a DurableInitialGrant. This slice adds
no continuing-authority or durable presence-recovery claim and no journal field.

Inventory local freshness charges the entire original send/receive interval and
never reanchors a replayed receipt. Presence responses are Recorded facts only,
not permits or installed/global claims. Network methods use the existing bounded
control lane, separate from bulk/private reads, with no automatic mutation retry.
The new code lives in http/scoped_presence.rs and a child of preparation/granted;
private material observation is a narrow owner method. Smoke is root-owned.

Qualification covers actual files/HTTP, positive and negative states, changed
record/bytes/scope, unsafe file types/modes/links, expiry/cancellation, immutable
retry/highwaters, control-vs-bulk progress and full upgraded package regression.
Synthetic runtime/provider qualifications remain explicit; production activation
and durable renewal/phase adoption remain separate.

Exactly one producer may be constructed per InputPipeline lifetime. Failed
construction releases its claim; a successfully established owner never allows
a new independent counter/map, even after the caller drops that producer.
