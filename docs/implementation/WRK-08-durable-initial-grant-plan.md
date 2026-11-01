# Durable received initial Grant owner

Base: worker `acac50d`. This component consumes existing exact admission0.1.7
contracts. It does not adopt the proposed accepted-preparation0.1.9 semantics,
remove the original preparation cap, launch a host, renew permission, settle
source/effects, release resource holds or resume a journal after restart.

## Durable history and process-local authority

`NativeClient::prepare` already returns an opaque in-process `Granted` after
strict request/plan/offer/nonce validation and conservative local lease/hard
projection. Add private initial sender/receiver CLOCK_BOOTTIME samples and
read-only accessors. Keep renewal samples distinct; this owner refuses a
renewal-bearing observation. No wire/Serde conversion is added to the opaque
network object.

Journal format5 adds one `ReceivedInitialGrant` per admitted attempt. It retains
actual initial response, canonical Prepared request association, initial network
samples, original lease and hard caps and an observation sampled before the
write. A timestamp serialized before fsync is not described as post-fsync.
The current-time observation after successful file+directory sync exists only
in the process-local return path. No recursive second timestamp commit is used.

Validation joins the actual durable reservation/runtime/qualification,
acceptance, captured preparation, Prepared intent, complete artifact/source
receipts and private-material pins. The public initial-grant validator checks
exact original grant/owner/material/hard-deadline association. Local projections
are recomputed from retained authenticated network samples, then bounded by the
original prepared/reservation caps. The original immutable grant, versions and
hard deadline cannot be substituted or extended by replay.

The existing descriptor-relative private writer, canonical checksum, eight-MiB
journal bound, file+directory fsync and poison-on-uncertainty remain. Formats1–4
are refused before pending cleanup or rewriting; there is no migration/reseed.
An old/reopened format5 record is inspection/stop-and-preserve history only,
never a fresh network observation or usable execution handle.

## Retained resource owner

`InputPipeline::record_initial_grant` consumes actual `PreparedInputs` plus the
opaque initial `Granted`. Inputs move into the pipeline's retained owner map
before validation/persistence can fail or the async caller can abandon a running
storage job. One admitted attempt owns at most one bundle; replay never replaces
it. `retry_initial_grant(attempt_id, fresh_opaque_grant)` uses that existing
bundle, rather than demanding a second exclusive workspace/source owner.

The map is bounded by admitted attempts/slots (maximum64) and measured actual
remaining artifact+startup bytes. Each bundle remains within the existing
8MiB inline-set bound, hence at most admitted-slots×8MiB (theoretical512MiB),
plus separately bounded metadata and one initial response per attempt. This is
a logical retained-byte bound, not an RSS guarantee or a production capacity
qualification. No additional replay bundle/network copies or arbitrary new
quota is introduced. Existing journal capacity may refuse a commit safely.

A cancelled waiter does not cancel an already admitted blocking storage job,
release its lane, erase the retained bundle, create replacement work or call a
host hook. Successful persistence followed by expiry records history but yields
no usable handle. A usable wrapper can be constructed only from the actual
in-process owners after post-fsync current-owner/deadline checks. It has no
Deserialize/Serialize conversion and no host-launch interface in this slice.
Every later effect would require a fresh check and a separately durable launch
intent; that integration is not implemented here.

Resource/file handles necessarily disappear when the entire process dies.
Durable journal/source holds remain, reopening provides no free capacity or
usable authority, and no destructor deletes resources or releases claims.
Errors and poisoned writers remain held/unknown. Lost server200 responses that
never produced an opaque client object remain Prepared/unknown; this owner does
not manufacture a Granted object from JSON or reconciliation data.

## Focused qualification

Use actual bounded local HTTP to produce opaque observations and the existing
actual filesystem/material/input join. Cover changed Prepared/grant/material/
source/runtime association, immutable replay/caps, expiry before/during commit,
all existing write fault cuts, poisoned writer, cancellation with retained
owners, restart inspection and byte-preserving formats1–4 refusal. Reuse the
explicit actual-Git fixture where feasible. Keep synthetic coordinator/model
qualification separate from real Git ownership and from any provider/host claim.
No services or network providers are required for normal tests.
