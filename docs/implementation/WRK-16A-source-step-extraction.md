# WRK16A shared source-transfer step extraction

Status: independently verified helper checkpoint, not native parking qualification.
Source baseline69abe91 with proposal c23dd93/598c45f. No native journal/schema,
production capability, ordinary result, coordinator route or new dependency is
changed by this checkpoint.

`source_transfer/steps.rs` extracts the existing bounded upload proposal,
progress acknowledgment and central receipt-association checks. Proposals return
metadata/bytes; the existing legacy Runtime still durably persists a new pending
chunk before returning its executable action. A missing/unconfirmed final
receipt never becomes retention. Final receipt association still binds the
separate transport UUID, actual Git artifact/retention handle, public canonical
Git manifest digest, repository generation and checkpoint object.

The helper deliberately creates no execution/stopped/retention authority and no
persistent store. The native continuation owner will call it only after its
separately verified stopped-workspace and journal step gates exist. Legacy
source result/history behavior is unchanged. Begin retains its prior no-root
behavior; begun legacy transfers still require their original managed root.

Executed using the root-released short Target2 lease at
`/tmp/aq-worker-artifact-overlap-target`,2 jobs, offline, incremental0 and dev/test
debug0:

- `cargo test -p aq-worker --offline --test source`:13 passed,3 ignored,0 failed,
  test0.51s after17.36s compilation. The skipped tests explicitly require the
  pinned real aq-git binary for workspace/CLI/HTTP qualification.
- `cargo test -p aq-worker --offline --lib source_transfer::steps::tests`:
  3 passed,161 filtered,0 failed, test<0.01s after10.81s compilation. New vectors
  distinguish a proposed state from durable state, reconstruct exact UUID/bytes
  after JSON history reopen, reject changed transfer identity/unconfirmed final
  status, and distinguish transport/artifact IDs and raw/canonical manifest SHA.
- `cargo clippy -p aq-worker --offline --lib --test source -- -D warnings`:
  passed11.08s. This is affected-target strict checking, not an all-workspace gate.
- Rustfmt on affected files and `git diff --check`: passed.

The retained source regression cases include socket-loss replay, stopped-writer
checkpoint fencing, fsync-failure preservation of the pending chunk and changed
file/symlink refusal. Their existing deterministic peers/fixture children are
not a new native tmux/real-Git/central-retention composition. The separately
prepared continuation-history scaffold was excluded from compilation and is not
qualified by these checks. No physical tmux/Git fixture or actual coordinator,
provider or canonical state was exercised. Target2 was released immediately to
the native journal owner after these checks.
