# Worker progress — 2026-11-17

Owner correction governs: machine -> enrolled nodes -> agent slots. Never collapse nodes.

Part 1: `ServiceConfig.nodes` loads the legacy `slots` key unchanged; new saves use `nodes`.
The node view of Layout uses `slots/<node>/worker.toml` for every agent of that node.
Agent 1 retains the exact legacy state and runtime paths. Additional agents use
`nodes/<node>/slots/<agent>` durable state and `<runtime>/<node>-<agent>` runtime roots.
No credentials are copied or retired. Two-node/three-agent tests assert six distinct roots,
sockets and host directories, and independent archival. Disk discovery preserves all eight
legacy nodes after an explicit node-count reduction.

Validation: CARGO_BUILD_JOBS=8 timeout 900 cargo test -p aq-node-service --locked
-- --test-threads=8: 62 passed, 11 failed, 1 ignored. Failures are socket creation denied
by this execution sandbox (EPERM), including the child aq-git fixtures.
Filtered `layout::tests`: 9 passed; `config::tests`: 11 passed.
CARGO_BUILD_JOBS=8 timeout 900 cargo test -p aq-worker --locked --all-features --bin aq-worker
-- --test-threads=8: compiles; 50 passed, 15 failed, 4 ignored. Socket fixtures are denied.
Logs in the private target/part1-*.log. No tests were skipped in code to hide restrictions.

Next: dynamic per-node supervisors, one native boot/registration and shared inventory,
manual occupancy, retire nodes above the configured count, packaging and final verification.

## Progress 2026-11-17 — node supervisor and native integration

Implemented per-node supervisors with live control growth/shrink, recovery of every retained
agent slot, explicit retirement of nodes above the machine count, node-specific registration
independent of recovery, one shared boot/registration operation, one inventory sequence and
publication gate, slot-specific poll headers, and local queued/manual permits. Legacy v1
control fallback is cached and retains per-attempt registration and one queued slot per node.

The supported aq-node-service run passes 71 tests (1 ignored; 11 socket-dependent tests
explicitly filtered after the full run demonstrated EPERM). The new tests cover two nodes
with three concurrent agents, live 1->3->1 capacity, held-slot isolation, six-node retirement,
register-once retries and mixed manual/queued capacity. Native header test: 1 passed.
Native six-journal identity/recovery/cross-slot attempt fence: 1 passed. Service tests without
socket fixtures: 14 passed. Workspace clippy with all targets/features and -D warnings: passed.
The original full worker lib run hit timeout 900 at the existing unrelated-fork-child test;
separate runs are validating the remainder. Exact final evidence will be in verification.md.

Sandbox limits: original Git index/ref/object metadata is outside the writable root, so
committing on the checkout's actual branch fails with read-only index.lock. The shared
handoff file also fails with read-only filesystem. A delivery Git repository under target/
and bundle/patches under handoff/ preserve logical commits without changing shared metadata.
Nothing was pushed. The actual checkout branch stays at the upstream wire baseline until those commits are
imported by a session allowed to write its Git metadata. That baseline is now fb9f19d, added
independently by the vendor owner after initial reading.

Remaining: finish packaging/documentation verification, full workspace test attempt under
900-second hard rule, release build and requested scratchpad copy; report any constraints.

## Progress 2026-11-17 — packaging and operator documentation

Installer and bootstrap accept --nodes (with --slots compatibility); new service files name
nodes, and receiver helpers read either key without changing credentials. New manual sections
omit max_slots. Receiver held-slot repair also recognizes nested node/agent state.
Operations docs explain eight nodes remain eight after update, explicit local reduction to
two followed by restart, and that binary-only remote updates preserve the count.
Packaging tests: 34 passed, 30 failed, 1 skipped; all remaining failures occur at the test's
Unix-socket fixture, EPERM. Shell syntax and Python compilation checks passed. No live service,
container, configuration or stack endpoint was touched. See open-contract-questions.md here
for the worker's exact contract choices and required server behavior.

## Progress 2026-11-17 — terminal correlation regression

Added an existing-binary regression using two valid stopped requests of the same node boot:
A's receipt is accepted for A and rejected for B, including after an operation-id substitution.
Command: CARGO_BUILD_JOBS=8 timeout 900 cargo test -p aq-worker --locked --all-features
--test native_http late_stopped_receipt -- --test-threads=8. Result: 1 passed.
The six-agent journal test also passes, and all 7 native-runner tests now pass. The explicit
supported service test run passes 14. The full workspace run is still pending at this point.

## Progress 2026-11-17 — release artifact

Release command succeeded (3m 23s): AQ_SOURCE_REVISION=3ec1755 CARGO_BUILD_JOBS=8 cargo build
--release --locked -p aq-worker -p aq-session-host -p aq-agent-client --bins.
Version: aq-worker 0.5.1+3ec1755. Worker path: target/release/aq-worker; identical executable
copied to the exact requested /tmp/.../scratchpad/rollout/worker-multi-agent/aq-worker path.
SHA-256: 5e0ce84f478f470d3bfc8808c551178f8bf00414ba264b3b67faaea669d8f006.
Final clippy, formatting and build-budget checks pass. See verification.md for exact commands,
per-item status and all outstanding integration limits. IMPORT.md explains how a writable
session can attach the preserved delivery commits to the actual checkout branch.

Final full workspace result: timeout 900, exit 124, at the existing unrelated-fork-child test.
Ninety test groups completed before the hung worker lib target; later targets did not run.
No full integration or production claim is made. Delivery is implemented with focused tests
and a built binary, but import into shared Git metadata and unrestricted integration checks
remain outstanding. No changes after the binary stamp affect runtime source.
