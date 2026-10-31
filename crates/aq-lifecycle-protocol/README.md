# AQ lifecycle protocol 0.1.0

This independently packaged schema-v1 crate adds observation/usage and suspended
continuation envelopes. It exact-depends on `aq-session-protocol =0.1.0`; neither
that published crate nor `aq-session-host` changes. New lifecycle capability IDs
are separately negotiated, so an old peer refuses them rather than treating a
suspended exit as ordinary success or failure.

The library has no process, tmux, HTTP, database, provider, Discord or worker-runtime
dependency. `ConformancePeer` is a bounded in-memory fake endpoint, not durable
admission, a usage ledger, actual stop verification or an execution permission.
AQ-owned wait registration, notification and operator contracts are separate in
`aq-lifecycle-api`; no new ordinary playbook command is defined here.

`Packet` binds schema version, stable message ID, exact required capabilities,
typed body and canonical SHA-256. Observation records additionally bind stable
source-event ID, sequence and body digest. Packet and receipt schemas are published separately; receipts pair exact message
ID/digest and only a batch acknowledgment can advance its delivered-through cursor.
Source provenance identifies native
session key, source generation, record/byte cursor, parser artifact/version/digest
and raw record evidence. Missing raw evidence is explicitly unavailable in a gap,
never a fabricated digest. Available raw byte length is positive and must equal
a byte-range cursor width. It does not impose a physical record-size policy. Binding includes project/task/lineage/attempt plus the
published exact remote node/boot/session/assignment identity. Central hosting uses
the explicit central authority identity rather than a fake worker.

Initial wire ceilings: 64 KiB per event/message, 256 KiB per transport batch,
32 events/batch, 32 references/held task messages/pending operations and 16 KiB UTF-8
summary. Logical text inherits the public 512 UTF-8-byte bound. Required capability
sets are at most eight; a fake peer advertises at most sixteen and retains at most
1024 message/event receipts. Real admission/spool/storage/CPU budgets and control
capacity isolation belong to the owning runtimes. No physical disk capacity or
invoice guarantee follows from an integer byte/cost claim.

Use `decode` for bounded untrusted JSON, and `Validate` at admission boundaries.
Serde rejects unknown fields and non-integral integer values. Generated schemas
publish structural limits and `x-utf8-maxBytes`; digest equality, UTF-8 bytes,
identity/cursor relationships, stop/retention consistency and capability negotiation
also require normative Rust validation. A schema-valid value confers no authority.
Canonical JSON uses the exact public session-protocol canonicalizer: recursive
sorted keys, compact UTF-8, ordered arrays and integer units.

Usage keeps known zero distinct from unknown. Every category is explicit, and a
pinned semantics reference says categories may overlap or are declared disjoint.
Cumulative counters carry their own positive generation; readers/ledgers must not
subtract across resets or source generations blindly. Provider/account/model/native
call identity, source-event identity and lineage support future deduplication and
precedence; the wire cannot prove absence of double billing. Parser correction is
a new event referring to the original digest, not a rewrite of the old observation.
`ReportedComplete` labels the observed report, not complete accounting when fields
remain unknown. Costs are unknown, provider-reported with receipt provenance, or
explicit non-invoice estimates with pinned rate card/allocation basis. No sums,
prices, hard spending enforcement or invoice settlement are implemented here.

Checkpoint text is an agent claim, not progress verification or stop proof. A
parking report has a distinct suspended disposition. Compute release requires a
reported stop; protected disk release remains a separate explicit directive/ref.
Retained-suspended shape requires source/checkpoint receipt references and effect
reconciliation. These remain claims whose exact owners, retention, process set and
current fences must be verified by runtime services. A stop report cannot by itself
close a graph task or consume a failure retry.

Continuation attachments bind predecessor, fresh successor attempt/session,
continuation/wait/outcome identities, retained-source receipts, fresh offer digest,
held task-addressed mail and a lineage ledger/budget checkpoint. The lineage UUID
is provenance/self-consistency, not an authoritative budget key or reset permission.
Central sealed attempt/continuation records must resolve the original project,
workspace and task aggregate; worker relabeling cannot reset retry/time/cost budgets.
They are not launch
grants. Old session-addressed mail and old credentials have no attachment fields.
Native provider resume is optional; fresh context plus retained source must work.
Bootstrap acknowledgment refers to the exact attachment and an authenticated AQ
ack receipt. A worker placing bytes in a prompt cannot manufacture that receipt.
Direct `Observation` packets are unbatched observation transport (also usable by
the central host). Their receipt has no worker-batch cursor and does not advance
that cursor, even if the observation itself carries a sequence. Only `Batch`
receipts advance the contiguous worker transport cursor. Future runtime adapters
must keep those transport semantics distinct and apply their source-event/call
identity deduplication; no billing or durable-ingress promise follows.

The fake peer demonstrates stale generation rejection and idempotent exact replay,
not actual authentication or lost-wakeup/process/ledger correctness.

`examples/export.rs` is the sole artifact generator. It writes packaged `artifacts/`
and the identical `protocol/lifecycle-v1/` mirror when run in this repository:

```sh
cargo run -p aq-lifecycle-protocol --example export --offline
cargo test -p aq-lifecycle-protocol --offline
cargo clippy -p aq-lifecycle-protocol --all-targets --offline -- -D warnings
```

An extracted package can run the same pure tests with an exact local public
session-protocol artifact override. No sibling runtime checkout or service is
required. Publication/qualification records identify the clean source revision and
archive digests separately; a passing fake endpoint is not product qualification.
