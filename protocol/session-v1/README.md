# Session v1 publication

Version: `aq-session-protocol =0.1.0`; schema version 1. The public crate is
independent of the foundation/execution packages. Existing execution and
foundation schemas, fixtures and digests are unchanged. Human profile/selection
and principal-free terminal requests belong to agent-q's session API contract.

The normative artifacts are `contract.schema.json`, `fixtures.json`, the crate's
validation rules and this document. Identical artifacts ship inside the crate,
allowing isolated consumers to test without this checkout. Generate both copies
with `cargo run -p aq-session-protocol --example export`. Consumers must pin the
package and fixtures, validate before admission, and test the negative cases.

Canonical JSON first validates the concrete type, converts to JSON, sorts object
keys recursively (including when Cargo feature unification enables
`serde_json/preserve_order`), then emits compact UTF-8 JSON with serde_json's
string escaping. SHA-256 lowercase hex of those bytes is the typed digest. Array
order is significant. Floats, duplicate struct fields, unknown fields and invalid
text are refused before canonicalization. Object key order and whitespace do not
change typed digests. Digest comparison never authorizes an untyped JSON object.
Raw output/snapshot payload digests cover the original bytes, not their numeric
JSON-array representation. These distinct digest domains must not be mixed.

`LaunchManifest` freezes catalog/modules, separate agent/intelligence/execution
profiles, project revision and public graph/task/source/outcome bindings,
concrete provider/model/reasoning and CLI/adapter/environment versions,
credentials by reference, requested/effective/required permissions and omissions,
context delivery/provenance and aggregate byte/token budgets, resource/stop
limits, mandatory `tmux` hosting and explanation provenance. Spending budgets
identify currency and whether task-cumulative costs are estimated or enforced;
absence means unspecified, while zero is an explicit zero budget. Enforced cost
requires the matching native budget capability; adapters must independently
qualify actual enforcement. Effective permissions must be requested and required
permissions must be effective. Required startup content cannot be deferred.
Bindings use a separate `graph_access_profile` artifact, never an agent profile.

`ProfiledOffer` is a new negotiated extension to the immutable execution spec.
Its `LaunchBinding` includes offer/attempt/generation, exact `SessionIdentity`,
existing execution-spec digest, canonical launch-manifest digest and existing
graph access-profile digest. Acceptance echoes the entire binding and advertises
exact supported capability versions. `validate_against` rejects changed bindings,
unsupported capabilities and expired grants. Grant and journal copy that same
binding; journal also binds the exact grant ID. A manifest never substitutes for
current execution authority. Old peers without the required profiled capability
remain connectivity-visible and explicitly decline the new offer. They cannot
accept by silently dropping this extension or by treating an access-profile digest
as a launch digest. No older envelope or fixture digest acquires new meaning.

Remote `SessionIdentity` maps `node_id` and `boot_id` to foundation NodeIdentity's
registered node and exact boot incarnation, and `attempt_id` plus
`assignment_generation` to execution AttemptRef's exact attempt and assignment
fence. `session_id` identifies one launch/session within that attempt and cannot
be reused for another pane. This is a semantic mapping, not a dependency on old
Rust types. Central identity instead binds `authority_id`, `session_id` and its
own monotonic generation. It fabricates no worker node, boot or assignment.
Terminal identity adds terminal ID and nonzero epoch; all UUIDs must be nonnil.
The central launch can use a central attempt for source/outcome bookkeeping; it
is never sent through a node batch or represented as a remote assignment.

Every manifest requires startup-context, confined-worktree and stop harness
capabilities. Every offer additionally requires profiled-launch and terminal
capabilities. Advertising only profiled-launch cannot accept a terminal launch.

Capability IDs in v1 are `aq.profiled-launch`, `aq.terminal`,
`aq.harness.startup-context`, `aq.harness.confined-worktree`, `aq.harness.stop`,
`aq.harness.native-message`, `aq.harness.resume`, `aq.harness.structured-usage`,
`aq.harness.transcript` and `aq.budget.task-cost`, each exactly version 1.
Unknown advertisements may be observed but never satisfy a required capability;
unknown required IDs and different versions explicitly refuse. Runtime adapters
must advertise only versions they have qualified. Registry changes require a
reviewed protocol publication, never permissive unknown-capability acceptance.

Controller leases bind principal, exact terminal/session, lease ID, monotonic
generation and execution-bounded expiry. A generation cannot change its lease ID
or principal. Renewals retain those identities. Permit lifetime is at most 15s;
input and resize deadlines are at most 2s and within that permit. Check current
identity, generation, lease and deadline again immediately before the effect.
Watch subscriptions never imply control; mailbox delivery is a separate operation.
Input is literal bytes only (at most 4 KiB); resize is a separate ordered setting.
No arbitrary tmux command or socket address is accepted.

The pure `InputConformance` oracle models same-ID/digest retries, changed-byte
conflicts, 32 KiB pending input and bounded receipt history (1024 operations).
A historical retry returns its existing receipt even after expiry or takeover;
that is readback only, and cannot transition it into injection. `begin_injection`
requires current authority and a Received record. Persist Injecting before the
actual handoff; uncertain handoff becomes DeliveryUnknown and is never replayed.
Subsequent input pauses pending explicit fresh-view resolution. The controller
requests a unique capture token after uncertainty; the snapshot must bind that
token and current controller generation. Quiet output or the same sequence does
not prove freshness. The real bridge must establish capture-request ordering and
authenticated authority; the oracle neither captures nor authenticates anything.
Receipt history exhaustion requires explicit retention/reconciliation rather than
unbounded memory or forgetting replay safety. `ResizeConformance` models separate
idempotent sequence ordering; it is not an exactly-once keystroke claim.

Output binds terminal epoch, sequence, byte length and raw-byte digest. A snapshot
binds snapshot ID, capture-request ID, renderer artifact, dimensions and inclusive
`through_sequence` watermark: replay begins at watermark + 1. Duplicate frames
already applied are deduplicated; changed bytes conflict; sequence holes require
an explicit gap and resynchronizing snapshot. An epoch change resets renderer
state and cannot overwrite a newer session. Snapshot state is opaque renderer
state, not a claim that capture-pane text reconstructs a TUI. Raw frame cap is
64 KiB; raw snapshot cap is 192 KiB so worst-case numeric JSON bytes fit within a
1 MiB wire document. Replay remains 1 MiB per session. A batch exceeding 1 MiB
serialized JSON is refused even when each member separately fits. Node aggregates
and renderer qualification remain runtime responsibilities.

Node envelopes carry node/boot, batch ID and cursor, multiplex bounded commands,
frames, snapshots, gaps and receipts and refuse any central or mismatched remote
identity. Polls hold at most 20s and contain no idle long poll without active work;
subscriptions cap at 32. `BatchConformance` models a bounded 1024-ID retry/conflict ledger.
Integrations must deduplicate identical batch ID/bytes
and conflict on changed bytes, enforce independent execution-control pools, four
viewers per session, per-node aggregate/rate limits and explicit gap/eviction
policy. These envelopes implement no listener, connection or retry transport.

Tests cover canonical/schema/fixture drift, strict mutation fields, unknown
required capabilities, launch/grant/journal digest fences, stale identities and
controllers, deadline rechecks, same-ID conflicts, uncertain no-replay, stale
capture refusal, literal backlog bounds, resize idempotence and snapshot/replay
boundaries. They establish no actual tmux, TUI, provider or durable-journal result.
