# Bounded inline artifact batches

The first real daemon/worker run downloaded36 inline artifacts using72 journal
snapshot publications. The optimized worker reached source hydration, but the
accepted preparation clock had only1328ms remaining and expired. This change
reduces avoidable journal work without changing any deadline or public protocol.

`InputPipeline::artifacts` admits the exact complete original descriptor set,
checks the8MiB total byte cap before network I/O, and persists every finite chunk
request/ID in one journal transaction. The bounded bulk lane then downloads and
verifies actual bytes. A second transaction records the complete receipt set.
No new fields or format migration are needed: format4's existing per-input
records retain their actual prior revision, capture and deadlines.

Existing incomplete plans reuse every original request/ID/deadline. A failed
network batch retains its plan and commits no new completion receipts. Existing
individually completed receipts remain historical; retry downloads actual bytes
again and does not rewrite unchanged metadata. Cancelled blocking work retains
the storage permit. An interrupted batch can expose only the old state or the
complete new plan/receipt set, and reopen remains historical-only with capacity
held. The individual artifact API retains its prior behavior.

This removes per-artifact snapshot/fsync overhead. It is not a cold-preparation
solution: the two-second selection cap still propagates into the accepted plan.
The separate preparation-phase design must establish a practical accepted lease
and fresh strict initial-Grant checks without extending old observations.
