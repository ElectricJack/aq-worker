# Worker protocol ownership

This directory is the canonical publication location for the worker wire contract shared by `aq-worker` and `agent-q`. The local candidate contains generated [execution schemas](execution-v2.schema.json), [conformance vectors](conformance-v2.json), preserved [foundation fixtures](foundation-v1.fixtures.json), and the [behavioral contract](contract.md). Canonical Rust types and validators are in `crates/aq-worker-protocol`; they generate the schema. This candidate has not been remotely published.

The [execution specification](../docs/specs/0002-execution-and-agent-access.md) defines local responsibilities. The [shared lifecycle](../../aq-server/docs/architecture/0002-execution-and-recovery.md) defines cross-system semantics, and [AQ's daemon specification](../../aq-server/docs/specs/0002-execution-and-agent-access.md) defines the server implementation.

Each protocol release must publish:

- Machine-readable request/response schemas and bounded field constraints for incarnation registration, offer/poll, acceptance, renewal, observation, directive delivery, result, artifact handoff and reconciliation.
- Authentication scopes, compatibility negotiation, stable error categories and retry/acknowledgement semantics.
- Valid and invalid examples, changed-payload ID conflicts, lost-response replay, stale-generation rejection and cancellation-race fixtures.
- A compatibility declaration and fake coordinator/worker fixtures usable without the other repository's implementation.

Optional Rust wire types derive from the same declared schema source or generate it; CI detects drift. They may depend on serialization/validation libraries, not daemon persistence or worker runtime internals. Both endpoints test against a pinned contract release. Cross-component contract changes require review from both consumers and providers.

Foundation enrollment, heartbeat and probe retain their version-1 semantics during extraction. The execution protocol has explicit version/capability negotiation; publishing these documents does not add execution fields to the existing routes or declare an API version shipped.

The current candidate package is `aq-worker-protocol` 0.2.0 (execution version 2).
Version 0.1.1 retains the 0.1.0 wire shape and strengthens optional-identity
validation plus fake-peer result authorization/retention tests. Connectivity
foundation types are independently preserved in `aq-protocol` 0.1.0.
