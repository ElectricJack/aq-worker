# Resumable source handoff (WRK-07 candidate)

Worker 0.3.3 adds an opt-in HTTP transfer consumer to the synthetic execution
driver. Use `run-execution --source-transfer` together with the three pinned
aq-git flags described in [source workspaces](source-workspaces.md). This
advertises `source-transfer-v1` as well as `source-workspaces-v1`. Without the
transfer switch, the driver retains the existing `source_pending` behavior.
The local repository binding must already be registered through aq-git's public
administration interface; assignments cannot register remotes or supply paths.

The new client uses the configured node credential and optional Cloudflare
machine headers over outbound HTTP. It has its own connection pool and a bounded
bulk lane, independent of execution control. Requests contain only protocol
identities, manifests, offsets and bounded chunks. Redirects are disabled; remote
error bodies and credential-bearing URLs are not reflected into diagnostics.
No new external service or runtime is required.

## Download and preparation

An exact registered binding may be admitted with a cold base. After acceptance,
the driver fetches the daemon's base-transfer manifest and verifies the frozen
attempt, specification digest and source binding. It reserves input bytes before
creating files. It requests chunks of at most 256 KiB into the private aq-git
incoming directory, fsyncs bytes before advancing the journal, then verifies both
file digests and the native Git manifest. Whole-artifact hashing runs on a
blocking task while the driver continues control exchanges.

Only a live, reconciled grant for the current incarnation permits import and
workspace creation. Download progress, complete bytes and a valid manifest do not
grant execution authority. A restarted node can recover retained bytes, but it
cannot revive the old grant or launch its old attempt. Input cleanup requires an
explicit non-start release and absence of create intent, workspace, launch or
unknown local Git operation. Unexpected directory members are preserved and
prevent successful cleanup.

## Checkpoint, upload and result

After the guardian proves stopped writers, checkpoint/export produces an
immutable local artifact. The journal reserves a separate transport UUID and
begin/finalize operation IDs before HTTP. At most one chunk descriptor is
pending; its offset, length, digest and operation ID are persisted before send.
Recovery reconstructs that exact chunk from retained files. Changed bytes,
symlinks, wrong associations and unexpected offsets refuse progress.

The client retries transient exchanges up to three times with the same request
identity. Backoff is bounded; a longer Retry-After, permanent refusal or exhausted
retry leaves the intent and bytes intact for operator/restart recovery. Neither
upload offsets nor an HTTP success count as central retention. A final receipt
must match the transfer, artifact handle, repository generation, checkpoint and
canonical native manifest digest.

Local report revisions created during checkpoint/export have never been sent to
the daemon. Before first delivery, the worker durably seals a fresh final report
with no remote predecessor. A lost response replays precisely that ID and body;
the sealed report cannot be changed by another export. Startup reconciles known
report acknowledgements before attempting a new delivery. Externally managed
legacy transfers do not acquire a fabricated first-delivery history: they need
their original owner/reconciliation path.

Central retention still does not authorize local deletion. The result response
must explicitly release retention, and the local stopped proof must remain
valid, before workspace retirement. Reconstructible repository caches remain
charged until separate conditional aq-git eviction succeeds. Unknown Git
operations and uncertain writers preserve their reservations.

## Control and limits

During network transfer and whole-file hashing, the synthetic driver polls for
directives with zero advertised free slots. Preparation also renews its existing
grant and checks cancellation/expiry. Upload after stop never renews a writer.
The transfer client bounds JSON responses to 64 KiB and each bulk chunk to
256 KiB. Status has a separate admission path. The default driver uses two bulk
permits, 256 MiB pending bytes and the existing conservative source disk profile.

This is not a filesystem latency or throughput guarantee. Journal fsync, bounded
chunk file operations and the existing local aq-git adapter are synchronous;
slow local storage can delay control. The synthetic driver runs one assignment
at a time, and this change does not qualify production harnesses, tmux agent
execution or fleet capacity. Actual transfer, retention and recovery results
belong in the WRK-07 qualification record, not in inferred claims from unit tests.

## State compatibility and verification

Journal format 4 accepts formats 1–3 without inventing transfer progress,
retention or launch authority. A retained format-3 reader refuses format 4
without changing its bytes. New upload/download descriptors and the sealed-report
marker are bounded fields in the existing journal; they are not a second state
database. Protocol 2 and its source-transfer messages remain unchanged.

Ordinary tests require no sibling checkout or network service:

```sh
cargo test -p aq-worker --all-features --locked
cargo clippy -p aq-worker --all-targets --all-features --locked -- -D warnings
```

They cover exact request replay after restart and failed journal commit, corrupt
and changed bytes, receipt mismatches, storage pressure, proven non-start cleanup,
first-report sealing, migration refusal, response bounds and separation of bulk
HTTP from control. Explicit pinned aq-git qualification is a separate check.
