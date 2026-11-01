# Installed adapter manifest and private material boundary

This component is a local input primitive. It does not implement Codex or another
harness, construct a command, establish readiness, grant execution, or activate a
worker run loop. `OpaquePrivateFileV1` names a handoff shape, not a supported
harness. Synthetic host profiles remain test-only.

`native::adapter::InstalledAdapter::open` checks a bounded operator-supplied
manifest against an independently selected canonical manifest digest and exact
adapter/runtime/qualification/renderer references. `AdapterManifest::decode`
checks the 16KiB limit before deserialization. The manifest contains no arbitrary
argv, hooks, environment or private material. Its executable/helper/tmux paths
must be absolute and canonical; retained descriptors must be regular executable
files owned by the current user or root, without group/other write or set-id bits.
Each file is bounded to 256MiB. Open and explicit `revalidate` hash exactly the observed length,
checks EOF, metadata stability and named device/inode identity. This detects
changes at verification time; it does not prove immutable bytes at a later exec.
These synchronous filesystem operations belong on a future bounded blocking
owner, outside control/renewal lanes. There is no elapsed-time I/O guarantee.

`InstalledAdapter::material` borrows the existing `InstalledMaterial`, matches
its complete receipt to the independent retained pin and exact launch binding,
and verifies actual private bytes. `MaterialHandoff` is neither serializable nor
Debug and exposes only the nonsecret receipt plus explicit zeroizing private-byte
access. Each access re-verifies the existing installed bytes (at most 64KiB). Material
handoff does not rehash unrelated executables; a future exec owner must invoke
explicit revalidation at its final boundary. It creates no
second store, journal, derived credential file, deadline or recovery protocol.
`MaterialStore::reopen_pinned` remains the historical filesystem owner; neither
that owner nor this handoff reconstructs live execution authority.

A future closed harness adapter must bind the exact installed manifest to the
actual sealed choice, map the material to isolated private harness configuration,
map bounded startup bytes to stdin, and establish genuine readiness/process-tree
ownership. Provider bytes must never enter `LocalCommand.environment`, argv,
`SealedLaunch`, `launch.json`, logs or node metadata. Node/supervisor credentials
must not reach the host. That adapter and the production polling/run loop remain
separate work. No provider requests are part of this slice.

Tests use ordinary temporary executable files (never run), an explicitly
synthetic material HTTP peer, and the actual existing private installer. They
cover independent pins, closed manifest fields, file replacement/content/modes,
symlink refusal, size bounds, exact binding/receipt matching and post-handoff
material tampering. No runtime, provider, sandbox or model qualification follows.

## Local checks

All cargo commands used `CARGO_TARGET_DIR=/tmp/aq-wrk08-preparation-target`,
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`, `--locked --offline`.

- `cargo test -p aq-worker --lib --all-features native::adapter::tests`: four passed (0.00s).
- `cargo test -p aq-worker --test native_materials --all-features`: five passed (0.28s), including the new exact handoff/tamper case and existing installer regressions.
- `cargo clippy -p aq-worker --all-targets --all-features -- -D warnings`: passed (7.98s).
- `cargo fmt --package aq-worker --check` and `git diff --check`: passed.

The initial unit attempt failed in test setup while writing a deliberately
read-only test executable; the fixture now explicitly changes mode before
mutation. No runtime or permission rule was relaxed. The final handoff test also
proves material access does not rehash executables: explicit executable
revalidation refuses changed bytes while the independent private-byte owner
remains readable. Future launch must separately require successful revalidation.
