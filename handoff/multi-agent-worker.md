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
Nothing was pushed. The actual checkout branch remains at d30c933 until those commits are
imported by a session allowed to write its Git metadata.

Remaining: finish packaging/documentation verification, full workspace test attempt under
900-second hard rule, release build and requested scratchpad copy; report any constraints.
