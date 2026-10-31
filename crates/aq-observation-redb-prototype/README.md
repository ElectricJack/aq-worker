# Candidate embedded observation store

This unpublished0.0.4 component adds bounded row-view normalization and a durable local transport-owner handshake to the isolated redb experiment. It is not integrated with the worker runtime. The qualified JSON reference and published session/lifecycle protocols remain unchanged. There is no legacy spool migration, production HTTP adapter, provider call, raw transcript artifact transfer or new command. The initial0.0.1 and bounded-delta0.0.2 checkpoints retain independent immutable qualification evidence.

The exact redb4.4.0 dependency (MIT OR Apache-2.0) is an embedded Rust library requiring no external service. Source/context/native/dedupe/outbox/event-ID/delivery rows share a private database and exclusive owner. Every write uses `Durability::Immediate` and two-phase commit. Candidate bounds are64 MiB physical,16 MiB logical retained values/selected keys,4 MiB cache,16 sources,65,536 identities,256 ordinary queued facts/2 MiB plus16 reserved gaps/64 KiB and32 facts/256 KiB per packet. No history is automatically evicted. Long-cycle COW growth, compaction and reserve sizing remain open; raw source retention and whole-directory disk bounds are separate obligations.

The backend checks private directory and file identity before writes. Files must be no-follow regular same-owner0600 single-link members. Positional writes/resizes cannot exceed the physical cap. Held database inode and root pathname identity must agree. Exclusive whole-file locking remains decisive if advisory owner-lock files are replaced. Existing files shorter than the pinned512-byte super-header are rejected before the engine entry point; missing/corrupt metadata and unknown formats are never reseeded. Startup audits the bounded retained projection; collection and small delivery operations do not scan unrelated history.

`aq-observation-engine0.0.1` supplies bounded source-scoped point/range reads and row deltas. A collection batch commits source cursor/anchor, parser context, dedupe rows and matching facts atomically. Partial/idle file reads do not commit. File-backed and public DTO sources have distinct cursor/provenance paths. Verified retained replay preserves old parser/fact references, never fetches arbitrary server paths and does not recharge replayed history. All four selected native released CLI formats remain unqualified; source fixtures are clearly synthetic and complete accounting is false.

Packets and retry metadata occupy separate rows. Prepare selects an exact bounded same-boot prefix using actual canonical record/envelope lengths, then seals once. Begin-send mutates only the tiny linked retry row. ACK validates the exact retained CON03 packet/receipt, deletes only that prefix and commits receipt/counters atomically. Unknown sends retain immutable packet identity. Process-exit and injected-error tests cover old-or-new projections, not hardware power-loss certification.

## Local owner handshake

`LocalIntent` consumes the exact frozen `aq-observation-admission0.1.0` contract. A trusted `AuthenticatedOwnerPeer` supplies fresh authenticated calls; credentials and network configuration never enter the intent. Deserialized receipts and status objects are recovery data, not create authority. This library has fake-peer qualification; a real mounted coordinator HTTP qualification is a separate gate.

One fixed owner root per node contains a bounded64 KiB intent, at most one64 KiB atomic temporary, an owner lock and the bounded database subdirectory. The caller must not let remote input select owner roots. Intent persistence uses a private held directory, exclusive no-follow creation, file fsync, descriptor-relative rename and directory fsync. Root/lock/current intent identities and bytes are rechecked. An uncertain persistence error requires reopen; retained temporary files hold for explicit recovery and are not silently deleted.

The exact Prepare request is durable before HTTP. Creation-start is durable before exclusive database creation, whose initial transaction embeds the immutable central marker and Prepared owner. Sources, collection and delivery are barred until activation. The exact Activate request is durable before HTTP. Lost activation replies can resume only an existing exact-marker store through a fresh owner call. Replayed initial receipts after central activation cannot recreate a missing store, even when accepted-through is zero. Empty/truncated/missing databases, corrupt intents and ambiguous incomplete creation preserve recovery holds.

A reopened owner must reconcile freshly before `with_active` exposes its managed store. Status requires the exact marker, monotonic owner proof, current host boot and a central cursor within the retained local prefix. It never creates a store or invents a missing boot fence. Public anonymous/reference-fixture open and old callback admission APIs cannot open or control managed stores.

Boot advancement blocks local collection before the durable request and remote call. Its immutable operation ID, expected owner revision/boot and next node sequence survive unknown replies. After exact replay, the local transaction fences old live sources, retains old packets/facts and updates the active boot without resetting node-wide sequence. A crash between remote advancement and local fencing holds collection until that exact intent converges. Existing immutable retained replay is separate from old live collection. Transport ownership supplies no execution lease, source registration, fresh-zero accounting baseline, parser qualification, account authorization or cross-session dedupe.

```sh
cargo test -p aq-observation-redb-prototype -p aq-observation-engine --offline -- --nocapture
cargo clippy -p aq-observation-redb-prototype -p aq-observation-engine --all-targets --offline -- -D warnings
cargo fmt --package aq-observation-redb-prototype --package aq-observation-engine --check
AQ_OBSERVATION_BENCH_REPETITIONS=90 cargo run --release -p aq-observation-redb-prototype --example measure_engine --offline
```

Measurement examples use synthetic inputs and no external services. Local latency varies with host/IO conditions; the evidence retains p50/p95/max and byte/sync/file growth samples. No production adoption, deployment or fixed performance guarantee follows from them.

## Finite HTTP owner peer candidate

`HttpOwnerPeer` uses pinned reqwest0.12.28 and only GET/POST on the fixed
`/v1/worker/observations/transport` route. A canonical bare HTTPS origin is
required; an explicit fixture flag permits literal loopback HTTP. Userinfo,
paths, queries, fragments, proxy inheritance, redirects, pooling, protocol-NACK
retries and automatic compression are disabled/refused. Requests and responses
are bounded16KiB, with3s connect/10s total timeout. Successful responses require
JSON and `Cache-Control: no-store`. Any network/status/body/protocol error leaves
the exact LocalIntent untouched; none is a claim that a mutation did not commit.
There are no automatic mutation retries or credential rotations.

The `owner_peer` example reads a private config file and a separate private node
credential file. Both require no-follow regular same-owner single-link files in
private canonical directories. The credential has no argv, debug, error, intent
or output surface. This node-owned peer supplies no agent credentials or source
admission. Cloudflare credential/header integration is not implemented here.

A synthetic config shape is:

```json
{"version":1,"origin":"https://coordinator.example","node_id":"00000000-0000-0000-0000-000000000005","boot_id":"00000000-0000-0000-0000-000000000006","credential_file":"/private/node/credential","allow_loopback_http":false}
```

The experimental example actions are `initialize OPERATION_ID STORE_ID`,
`prepare`, `create`, `activate OPERATION_ID`, `resume`, `advance OPERATION_ID` and
`recover-boot`. The example receives only config/root/action/UUID arguments,
prints bounded stage/sequence metadata and never generates a replacement intent.
Its default limits are the candidate bounds above. It is not a production worker
command or execution adapter. Local synthetic HTTP tests and separately invoked
actual daemon/Postgres qualification must remain distinct in the evidence.
