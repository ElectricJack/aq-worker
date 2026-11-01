# Separate proposal freshness from accepted cold preparation

**Design proposal only.** This document enables no authority, changes no clock,
and does not close cold-preparation qualification. The frozen smoke source is
`4eef294`; its debug and optimized release binaries both failed safely against
the actual daemon under the existing phase rules.

The measured release probe had a 1,328 ms immutable preparation window after
471 ms Poll and 67 ms Accept handling. All 36 original artifacts, material and
source status/manifest/bundle reads returned 200; the worker refused in hydration,
with server time 43 ms beyond original preparation expiry. Debug probes also
refused during artifact collection or before reservation (Poll 409). Thus this
is not merely a debug-build issue or proof that batching alone closes cold
preparation. A bounded two-transaction inline batch remains a useful independent
optimization; it must not change authority lifetimes.

## Three distinct boundaries

1. **Proposal freshness:** existing short-lived selection/budget observations
   establish whether the exact sealed proposal may be timely accepted. Their
   expiry remains immutable. Prebuild/cache immutable definitions where useful,
   then take genuinely fresh observations near the admission fence; never give an
   old observation a newer timestamp. A proposal expiring before publication or
   Accept is refused, without automatic replacement or implicit fallback.
2. **Accepted no-launch preparation:** successful timely Accept may atomically
   issue a separately versioned, immutable preparation lease. It pins the exact
   project/task CAS/content revision, node/boot/session/attempt/generation,
   execution spec/manifest/context digests, sealed choice, original proposal and
   acceptance operation, reservation/guardian/launch identities, input/material
   scope and owner revision. Its separately configured absolute deadline permits
   only bounded original artifact retrieval, private installation and qualified
   Git import/workspace preparation. It grants no host or model permission.
3. **Fresh initial Grant:** after actual input preparation, current owners must
   confirm the exact sealed choice is still eligible. A new confirmation links
   historical immutable selection/context/source to newly observed current facts;
   it does not rewrite that selection, silently switch account/model/source,
   substitute an artifact or revive an expired execution permit.

A local reservation's resource **hold** is separate from either deadline. All
uncertain/admitted attempts remain charged until explicit reconciliation and
release proof. Expiry never creates free capacity, proves stopped writers or
permits a new launch ID.

## Accepted-lease issuance and bounds

Accept must still pass the current authenticated principal/runtime, exact current
boot/generation, task claim/CAS, project/registry and proposal-freshness fences.
In one transaction it records the immutable accepted lease, exact request digest
and receipt, preparation-only input/material scopes, hard preparation duration
policy and all budget/resource charges. Same-ID replay returns the original
receipt; changed payload or a closed/revoked generation refuses. It never moves
its deadline forward because delivery or recovery happened later.

The first profile should qualify a finite cold window, for example a configured
30-second maximum, with a smaller bound where actual retained inputs or owner
policies demand it. This is a proposed new policy, not permission to widen an
existing 2-second plan. Publish and qualify the chosen duration against transfer
sizes, network/storage deadlines and concurrent control. Larger cold workloads
may need prewarming or a separately designed owner-revalidated preparation
continuation with a fixed cumulative ceiling; automatic extension is excluded
from the first slice.

Audit every original bound: retained artifact/context availability, source
admission and Git retention, material retrieval lease, reservation, selection,
budget observation and any project policy. Existing SourceAdmission can expire
within 30 seconds and material retrieval within 120 seconds. Do not reinterpret
those old expiries. If the new phase needs a fresh source eligibility confirmation
for the same immutable object, define that owner artifact/relationship explicitly;
never retime the original source observation or re-resolve a named ref to a new
base. A policy requiring the latest ref must freshly confirm the pinned base or
refuse. Missing retention/current-source evidence is not renewed by a lease.

## Strict Grant confirmation

Reuse the current-fact owner's pinned/current/observations split. Load original
sealed spec, exact choice, accepted lease, actual installed material receipts and
policy/budget provenance under owned locks. Obtain bounded fresh external facts
without authority locks, then recheck their exact associations and freshness in
the final transaction after every lock/quota wait:

- current node/runtime/config/boot/generation and still-open accepted preparation;
- same current project revision, catalog/profile permissions, registry and
  credential profile/revision/provider/account/endpoint/node binding;
- current installed principal and exact qualified harness/model inventory;
  newer explicit missing capability or credential evidence denies;
- fresh actual budget and initial-placement availability; provider negative or
  cooldown denies, and stale/absent availability is **Unknown and refuses**;
- exact task CAS/content and full original scoped context through the Graph
  owner, not a new version number paired with old content;
- configured repository logical identity and fresh active retention for the
  exact original object, plus the current source eligibility policy;
- actual private-file fingerprint and original immutable installation claim.

Renewal's narrowly permitted Unknown continuation rule applies only to an already
held, still-live, unchanged grant. Initial Grant cannot borrow it. Historical
installation receipts remain checked at the original issuance time; retrieval
lease expiry is not proof that installed bytes disappeared, and a lease alone
cannot reconstruct a missing installation receipt.

Grant must commit its fresh confirmation/pinned fact references and exact
installed receipts atomically with the original grant, current authority versions
and quota charges. Its execution/hard deadline is bounded by actual remaining
policy and the new phase's defined current-admission rules. No old selection or
budget snapshot is relabeled as the new fact. Any missing/changed pin or current
fact refuses and preserves holds.

A new execution lease also needs an explicit transition clock rule: preparation
expiry gates **server admission**, while the newly committed execution lease has
its own fresh delivery/hard deadline. Carrying the old preparation cap into that
new permit recreates the observed server-200/client-expired edge. This requires
new negotiated contract semantics; do not remove the frozen 0.1.7 plan cap in
place. Prove exact accepted revision and issuance before preparation closes,
never revive a prior expired execution permit, and retain committed-but-unusable
Grant delivery as unknown/held. A network 200 alone is not a live worker permit;
a local refusal after 200 is not NeverGranted.

## Public and worker compatibility

Do not silently change the frozen admission 0.1.7/0.1.8 behavior. Current workers
cap PreparationDelivery to their original reservation deadline; journal formats
1–4 preserve those exact sampled clocks, and InitialGrantDelivery also caps grant
expiry to the original plan. A server-only ceiling change cannot confer a usable
new phase and would obscure compatibility.

Publish an additive exact-versioned accepted-preparation contract/capability and
explicit negotiation. Unsupported peers refuse that profile; no automatic legacy
fallback after partially adopting it. Audit schemas, source/material endpoints,
renewal/closure interactions and human inspection before selecting exact versions.
Keep existing published archives/fixtures unchanged. Existing profiles continue
their qualified legacy behavior; a new runtime/profile advertises the new phase
only after independent owner/worker qualification.

A new worker journal format retains the original proposal/reservation clocks and
the separately admitted preparation receipt/clock/revision. It computes one
conservative local CLOCK_BOOTTIME deadline from the actual authenticated accepted
delivery interval. It does not modify the earlier deadline or anchor a cached TTL
to a later retry. A store opened after restart remains history/stop-and-preserve
unless a separately qualified owner recovery handshake proves otherwise; there
is no format migration, reseed, old-boot relabeling or fresh proof from JSON.

The real input owners, exact request IDs, private pins, source retention/workspace
checks, bounded spool, 8 MiB inline join and fsync-before-send rules remain. The
later physical host launch intent must precede every host/adapter hook. New
preparation permission is never a launch permit.

## Cancellation, cold source holds and qualification

Control/renewal lanes remain independent of download, fsync and Git jobs. A
cancelled/timed-out caller cannot release the blocking job's owner or spawn a
replacement. Check deadline/revocation before new I/O and after possible effects;
ambiguous writes/import/create retain exact intents and resource/source holds.
Do not truncate unknown input tails or infer completion from a staging path.

An actual central NeverGranted closure must serialize against Grant issuance;
ever-issued or unknown Grant remains a distinct held state. Only after exact
closure plus qualified no-host/physical stop facts may cleanup proceed through
actual Git retention release/workspace retirement with expected identity,
generation and HEAD. Unknown Git or host effects block cleanup. No automatic
recursive deletion, task completion, source reuse or account rotation follows
preparation expiry.

Required tests include timely Accept versus proposal expiry; cold setup beyond
proposal expiry but inside the new lease; immutable replay and lost delivery;
all phase crash cuts; unchanged old format/clock refusal; cancellation/revocation
and quota waits; strict fresh budget/availability/context/inventory/source
refusals; bounded concurrent cold transfers; unknown Git effects; Grant/closure
races; and exact package/feature negotiation. Qualify actual daemon + independent
worker process + actual Git in both cold and warm cases. Report each window and
physical bound honestly; a single optimized tiny-fixture pass is not closure.
