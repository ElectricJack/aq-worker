# Received initial Grant owner qualification

Source `99e793c`, over artifact-overlap `024ff2f` (`a39e990` equivalent), design
`9e43fe5` and worker base `acac50d`. This is a component checkpoint, not a release,
production driver adoption or host execution qualification. The package remains
aq-worker 0.3.3 and consumes unchanged aq-admission 0.1.7.

The exact source passed:

- Full worker package: 164 passed, zero failed, five explicit opt-in Git cases
  ignored, across 22 test processes. Summed test-process execution 18.14s;
  build 14.06s. The ignored cases are the two native hydration/grant cases and
  three existing source/CLI/HTTP actual-Git cases.
- Focused received-grant owner: 12 passed in 5.17s. Four fsync fault cuts and nine
  association/clock tamper subcases are included within named cases.
- Explicit actual-Git: 2 passed in 4.00s, the new retained-grant owner and the
  existing hydration/retention profile. Exact public binary SHA256 is
  `c2ff26f5e63008aec0fbd4082a547e87825e651d4aba0744c4b9fdf752d58c39`.
- Strict worker all-target/all-feature Clippy passed in 29.93s; workspace
  formatting check passed. No total wall time was measured.

All checks were offline with an isolated target `/tmp/aq-worker-grant-target`,
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`; tests declared `RUST_TEST_THREADS=2`. Debug-symbol
removal leaves assertions enabled. Full package command was
`cargo test -p aq-worker --all-features --locked --offline`. Strict lint was
`cargo clippy -p aq-worker --all-targets --all-features --locked --offline -- -D warnings`.
The opt-in command selected `cargo test -p aq-worker --lib pinned_git_
--all-features --locked --offline -- --ignored` with the explicit binary path
and SHA256 environment variables recorded in qualification metadata.

Actual private files, descriptor ownership, HTTP sampling, canonical journal
checksums, file/directory fsync, runtime/Prepared/material/source association and
real Git import/workspace inspection are exercised. Coordinator and runtime/
provider qualifications are synthetic. Normal tests need no provider or external
service. The new coordinator fixture's declared one-second Grant and 100ms expiry
cases are test profiles, not widened production grants or saturated capacity
claims. Expiry tests wait for the actual sampled CLOCK_BOOTTIME deadline across a
controlled fsync barrier; they do not retime observations or relax assertions.

The component retains actual inputs synchronously before its returned future
can be dropped. Cancellation while fsync runs leaves the bounded resource map
and lane owned by the live pipeline. Changed request/receipt, private bytes or
current source refuses usability while preserving history/holds. Same-grant
retry preserves its first receipt and clock caps, with no duplicate bundle.
Formats 1–4 are refused before any old schema interpretation/pending cleanup;
their opaque original journal and pending bytes are unchanged. Format 5 reopen
remains inspection/stop-and-preserve: no free capacity or live permit returns.
A serialized timestamp is explicitly observed-before-write, while the actual
post-sync sample exists only in the non-Serde process-local result.

The retained input-body bound is admitted slots×8MiB, theoretical 512MiB for 64
attempts; startup bytes are included rather than double-counted. The existing
4MiB control envelope and 8MiB aggregate journal cap remain. These are logical
bounds, not measured RSS/disk guarantees or a production capacity envelope.
No host hook, launch intent, renewal, source/effect settlement, release, reset or
cleanup API was added. Later driver adoption must persist a separate launch
intent before every host/adapter effect and independently qualify the runtime.

The earlier actual server 200/client-refused delivery cannot construct opaque
Granted. It remains durable Prepared/unknown, never NeverGranted. This owner can
pin a once-usable opaque observation that expires during commit, but does not
claim every ever-issued or abandoned/busy network reply is locally pinned.
Reconcile/closure and the separately versioned phase transport own that remaining
boundary; no admission 0.1.9 bytes or semantics are adopted here.

Retained development preflights include unused draft-function/import warnings
and a compile failure from tests requiring Debug on deliberately opaque types.
Those tests now match error variants without adding Debug/Serde to authority or
private owners. There were no runtime failures in the focused, actual-Git or
full package runs. Raw logs, exact source inventory, toolchain and frozen archive
hash are in [evidence/worker-initial-grant-99e793c](evidence/worker-initial-grant-99e793c).
