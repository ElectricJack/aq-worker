# Lifecycle v1 public contract

The editable source is `crates/aq-lifecycle-protocol`; its exporter generates the
schema/fixtures here and identical copies in the standalone package's `artifacts/`.
Do not edit generated JSON independently. The crate README defines normative bounds,
semantics and limits. This adds a separately versioned capability namespace while
preserving published session protocol/host 0.1.0 bytes and APIs.

Observation receipts, source/wait outcomes, physical suspension, fresh wake admission
and bootstrap acknowledgments remain separate. Runtime authentication, verified stop
ownership/retention, ledger accounting, lost-wakeup handling and actual transports
are later implementation tasks. No playbook command is added by this contract.
