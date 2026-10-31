# Execution protocol 2 candidate (aq-worker-protocol 0.1.1)

Rust wire types in `crates/aq-worker-protocol` generate `execution-v2.schema.json`.
Run `cargo run --locked -p aq-worker-protocol --example schema` to regenerate;
conformance tests compare the checked-in bytes. Request bodies contain the named
request struct directly. The `WireMessage` envelope exists for fixture/schema
cataloguing only. Foundation v1 routes and payloads are unchanged.

All execution requests use the enrolled node's Bearer credential. The server
must match credential node, registered boot and fenced attempt generation.
Node credentials permit only this node's control, known-attempt recovery and
artifact handoff. They never authorize agent graph writes.

POST `/v2/worker/register` negotiates version 2 and capabilities before work.
POST `/v2/worker/poll` returns at most max_items offers and directives each,
never more than 64. Offers are not authority. Accept, decline, renew, observe,
directive-ack, result, reconcile and artifact-handoff use corresponding POST
paths. Registration cannot implicitly replace an active conflicting boot.
Every boot change is a reconciliation boundary.

Stable operation IDs bind canonical typed JSON SHA-256 digests: object keys
sort lexicographically; arrays retain order. Retry identical operations after
lost responses. A changed payload is HTTP 409 `id_conflict`. Exact replays
return the committed disposition; expired/revoked authority is never revived
by replay. HTTP 426 signals unsupported version, 400 invalid bounds, 401
invalid/revoked credential, 409 fencing/identity/expiry conflicts, 429 bounded
overload (retry), 503 transient failure. Errors contain ProtocolError. Only
explicit retryable=true permits backoff; transport failure is uncertain.

Offer expiration is daemon epoch milliseconds and daemon authoritative.
reservation_deadline_ms is the worker's local suspend-aware clock deadline,
scoped to boot; it is opaque to the daemon. The grant binds that reservation
ID. A worker checks its local deadline again after receiving any grant.
Grant valid_for_ms is anchored conservatively to the local request-send time,
not response receipt. It cannot exceed 120000 ms. A renewal received after
local expiry cannot restore authority. Restart invalidates cached deadlines
and requires reconciliation. Server grant expiry remains authoritative.

Observation sequences increase per attempt. Repeating sequence/operation with
different bytes conflicts; lower unknown sequences are rejected. Observation
acknowledgement means durable receipt, not execution authority or task success.
Cancellation acknowledgement `received` does not mean `stopped`. A report is
immutable, retains the spec/session/generation binding, and has independent
process outcome, writer certainty, agent claim, artifacts and unresolved IDs.
Report disposition acknowledgement does not mean graph completion. Retained
cancelled/stale reports cannot settle another attempt. Artifact transfer offset
acknowledgement does not imply central retention: only central_receipt does.
retention_release=true alone permits local artifact retirement after writers
are confirmed stopped. Cancellation arriving before report admission yields
retained_cancelled; accepted report admission first retains its accepted fact.

Bounds are checked by Validate at both endpoints as well as parsing. UUID
identities cannot be nil; generations and sequences are positive; capabilities
are unique strings <=128 UTF-8 bytes, at most 32; page size and retained arrays
are <=64; details/output <=4096 bytes; digests are lowercase SHA-256 hex.
The JSON schema captures shape; semantic bounds are canonical Validate code.
Current execution window is [2,2]. Connectivity-only foundation window is
[1,1] and does not imply execution support. Unknown required capabilities decline.

Conformance fixtures and fake coordinator tests are behavioral evidence, not
production database or process-isolation evidence. Package consumers vendor an
immutable generated source package and record its source revision and digest;
no sibling source dependency or floating Git branch is required.

Recovery-before-registration is explicit: a new process first reconciles known
old attempts under node recovery scope and replays immutable stopped reports.
The daemon may grant an exact new-boot registration permit only after checking
all previously known writers are stopped or never started; omitted local entries
cannot establish that proof. Conflicting direct registration still fails. The
worker never uses this flow to relaunch an uncertain attempt. Observation
sequence/origin is implemented; a separate observation timestamp is reserved
for a subsequent reviewed contract extension.
