# aq-agent-api 0.1.0

Agent Q owns this standalone Rust consumer artifact for agent-access API 1.
The embedded schema, OpenAPI, CLI contract and fixture bytes are copied exactly
from the public API-01 contract; `artifacts/inventory.json` records their hashes.
The daemon and worker client use the same pure shape validator, strict JSON
decoder and canonical digest rules. There is no database, daemon dependency,
network client, token handling or graph authority in this crate.

`Binding` and `Context` validate exact scope assertions and the access-profile
and graph-snapshot digests. These are different from an execution profile and
composed startup context. A cached Context does not authenticate a credential,
renew a grant or authorize an offline mutation. Live authorization remains at
the daemon on every admission.

`check` implements only the vocabulary used by the trusted retained AQ/graph
schemas. It is not a general JSON Schema implementation and must not be used
with caller-selected schemas. `valid` rejects unknown public type names.
Transport consumers must additionally enforce response bindings, route-specific
semantics, bounded reads and the live authority/retry rules in the public contract.

Run `cargo test -p aq-agent-api --locked` independently of sibling services.
