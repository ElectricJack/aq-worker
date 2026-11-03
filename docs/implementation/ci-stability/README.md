# Main CI test stability (sharp-forge-44)

The failures were traced to the logs of Actions runs 41586009029,
41585962839, 41585891999, 41581711906 and 41580367447.

- Supervisor startup delivery: the shell created `delivered` before `cat`
  wrote its payload. Publish the completed file with a rename, so existence
  is a readiness signal. Both startup tests retain their exact byte assertions.
- Sandbox handoff receipts: the fixture agent could exit before tmux and the
  host recorded launch. Hold it until `sandbox-run/started.json`, then release
  it. Missing-source, missing-result and complete receipt assertions remain.
- Journal migration: the frozen v1/v2 readers only close their flock descriptor.
  A concurrent fixture fork inherits the open file description, briefly retaining
  the lock after the reader is dropped. Coordinate those reader lifetimes and
  the forking test with a mutex. Frozen readers and migration assertions remain
  unchanged.
- Embedded template tools: Rust 1.96.1 can concatenate the public Serde labels
  in either observed order. The second order was reproduced in the local worker
  binary. Recognize that exact context too; token, assignment and URL rejection
  tests still cover both metadata forms.

The existing `check` workflow now accepts manual dispatch, allowing independent
runs of the same delivered main commit without additional source changes.

Validation uses Rust/Cargo 1.96.1, `CARGO_BUILD_JOBS=2` and
`RUST_TEST_THREADS=2` through a foreground `aq test` wrapper. The repeated
migration target includes the forking test to exercise the original concurrent
lock scenario; the template target runs all three acceptance tests.

The 20-round run used startup base `4992a16`. All 20 rounds passed: 100 cargo commands and 220 Rust test invocations, including
1,000 launches in the additional supervisor startup stress test. Exact commands,
results and raw-log hashes are in [verification.log](verification.log).
An initial startup stress run hit the existing three-second guardian identity
timeout at iteration 25; its log is retained as
[initial-startup-timeout.log](initial-startup-timeout.log). The immediate recheck
and all 20 final invocations passed. Launch deadlines remain unchanged.

After syncing with main at `7979083`, the related targets passed once more:
29 Rust tests and 19 Python tests. Commands and results are preserved in
[post-sync-area.log](post-sync-area.log).

`cargo fmt -p aq-worker -p aq-session-host -- --check` passes for both affected
packages. `cargo fmt --all -- --check` cannot resolve the nested worktree's
vendored packages against its outer checkout; the standard full formatting
check remains in CI.

Three consecutive `check` passes on the delivered main commit remain the
integration owner's acceptance gate. The worker publishes its task branch and
does not independently merge or push main.
