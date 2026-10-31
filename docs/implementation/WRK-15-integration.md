# Observation component integration

Worker source `5a41740` integrates the independently qualified observation engine,
indexed redb experiment and private transport-owner HTTP peer alongside the
existing worker and scoped graph client. The merge preserves the WRK-05 client
and its pinned API package. It adds no daemon source or external service build
dependency. The production `aq-worker` runtime does not yet adopt this store.

The clean committed integration passed 271 workspace tests with all features;
19 declared optional qualification tests remained ignored. Strict workspace lint
with all targets/features and formatting passed. [The JSON inventory](WRK-15-integration.json)
records source and log hashes. The component's stronger fault-injection,
standalone-package and actual HTTP evidence remains in the
[delta](WRK-15-delta.md), [local owner](WRK-15-local-intent.md), and
[HTTP peer](WRK-15-http-owner.md) reports.

The engine applies bounded indexed changes instead of cloning complete retained
history on every collected record. Its redb owner preserves exact packet bytes,
source identity and accepted-prefix history through retries and crashes. This
integration does not qualify a production harness, implement source catalog
admission or claim end-to-end accounting. Managed physical-capacity measurements
remain separately tracked; no history eviction or limit increase was introduced.
