# Worker protocol ownership

This directory is the canonical publication location for the worker wire contract shared by `aq-worker` and `agent-q`. It currently records the design requirements; executable schemas and conformance fixtures are implementation deliverables and are not yet present.

The [execution specification](../docs/specs/0002-execution-and-agent-access.md) defines local responsibilities. The [shared lifecycle](../../agent-q/docs/architecture/0002-execution-and-recovery.md) defines cross-system semantics, and [AQ's daemon specification](../../agent-q/docs/specs/0002-execution-and-agent-access.md) defines the server implementation.

Each protocol release must publish:

- Machine-readable request/response schemas and bounded field constraints for incarnation registration, offer/poll, acceptance, renewal, observation, directive delivery, result, artifact handoff and reconciliation.
- Authentication scopes, compatibility negotiation, stable error categories and retry/acknowledgement semantics.
- Valid and invalid examples, changed-payload ID conflicts, lost-response replay, stale-generation rejection and cancellation-race fixtures.
- A compatibility declaration and fake coordinator/worker fixtures usable without the other repository's implementation.

Optional Rust wire types derive from the same declared schema source or generate it; CI detects drift. They may depend on serialization/validation libraries, not daemon persistence or worker runtime internals. Both endpoints test against a pinned contract release. Cross-component contract changes require review from both consumers and providers.

Foundation enrollment, heartbeat and probe retain their version-1 semantics during extraction. The execution protocol has explicit version/capability negotiation; publishing these documents does not add execution fields to the existing routes or declare an API version shipped.
