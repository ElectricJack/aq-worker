# Native durable input preparation slice

Base worker `281f8a4` retains no-launch reservations, exact Accept and observed
preparation, but no durable fetch operations or installed-material pin. This
slice adds native journal format 2 and a bounded input pipeline. Format 1 is
refused unchanged; it is historical evidence for the prior qualified inspector,
not an empty node or an automatic migration. Reopened journals remain strictly
stop-and-preserve and cannot issue fresh network or preparation authority.

Before artifact I/O, persist the exact original descriptor and all bounded chunk
requests/operation IDs as one atomic plan. Retrying an incomplete download reads
the same requests again; completion retains a verified whole-artifact receipt,
never artifact content. Inline bytes remain RAM-only: a retained receipt is
historical verification metadata, not durable payload storage. Losing the returned
VerifiedArtifact requires re-downloading and re-verifying the same requests; no
receipt reconstructs bytes or satisfies a future Prepare gate by itself. Planning
and completion each use one journal transaction,
not a snapshot rewrite for each chunk. A failed possibly effective write poisons
the writer. Original preparation delivery/clock provenance travels with the plan;
refresh cannot extend the captured deadline. Counts and encoded state stay
bounded; accepted unknown attempts never free capacity.

Before private material retrieval, persist the exact MaterialFetch request.
The private material owner installs/fsyncs bytes, then the journal persists its
exact independent installation record/pin. No private bytes enter the journal.
Historical reopen must verify that external pin plus exact worker/lease/request,
record digest and actual bytes; a self-consistent replacement is insufficient.
Neither a reopened installation nor a receipt reconstructs a live delivery.

Blocking filesystem work uses one bounded storage lane; whole downloads use a
bounded bulk admission separate from renewals. Synthetic HTTP tests prove request
identity precedes I/O, changed responses/refusals, lost replies, durable pin and
reopen corruption behavior. They do not qualify native execution or physical
power-loss/ENOSPC behavior.

Prepare remains withheld. Frozen 0.1.7 requires exactly all original inline
artifacts (at most 126) plus actual source-manifest/source-bundle receipts, with
no duplicate digest or missing-ref substitution. The source refs are
`aq.source-manifest/{transfer_id}@1` and `aq.source-bundle/{transfer_id}@1` with
actual TransferManifest sizes/digests; transfer_id equals artifact_id/export ID.
A later slice must persist public Git import/workspace intents, verify actual
confined hydration and retain its owner proof before adding a Prepare gate.
No public constructible fake-hydration marker, runnable registration, host hook,
launch, grant reconstruction or result settlement is added here. Later launch
intent must precede invoking any host/adapter preparation hook, not merely the
existing later callback. Granted-without-Started remains held pending its separate
closure protocol.

Current logical bounds are 126 inline records/32 material records per attempt,
1024 input records and 4096 wire requests per node, with the existing 64-attempt/
8 MiB encoded snapshot cap. All retained attempts remain charged. These bounds
can conservatively refuse a later input operation after acceptance; no slot is
freed on that refusal. The journal still rewrites its bounded snapshot per plan
and per completion, so this slice makes no history-insensitive throughput or
physical disk/headroom claim. Artifact payloads are limited to 2 MiB in RAM and
chunks to 256 KiB; actual Git bundles remain a separate streaming owner. Private
material limits remain those of the existing material owner. No migration from format 1 to 2, blanket cleanup, new service or runtime dependency is added.

The subsequent [native hydration slice](WRK-08-native-hydration-slice.md) uses a
new explicit format 3; this format-2 qualification remains a frozen historical
checkpoint rather than an upgrade or current-format compatibility claim.
