# C7 Boat Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking. Execute inline in this assigned AQ worktree; task completion/publication follows AQ prime.

**Goal:** Run grant-bounded attempts in boat VMs, preserve stopped disks, and refuse unavailable capacity without orphaning compute.

**Architecture:** Add an HTTPS boat client and a PlacementBackend adapter with an fsynced node-local intent/mapping journal. A saved snapshot materialization binds an allowlisted OCI digest to an immutable scanned snapshot; attempts create from that snapshot, then run the OCI container with the existing sandbox security policy. Controller failures retain uncertainty and expose a typed backend-unavailable refusal. Each enrolled node chooses exactly one backend.

**Tech Stack:** Rust, reqwest, Tokio, serde, existing Python C3b scanner, axum HTTP double.

**Spec:** calm-delta-18.11 complete task description; aq-server/docs/architecture/0005-sandboxed-execution-and-deployment-topology.md §4.1/5.2/14; C1 PlacementBackend and conformance.rs; C3b packaging/sandbox/template_tool.py and credential_scan.py.

## Global Constraints

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- **Naming:** while v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- Owner velocity rule: run the named focused tests and related area checks; no full qualification/full-suite reruns. A live test is opt-in and requires approved node-local credentials. C7 is not marked adversarial.

### Task 1: HTTP client, durable mapping and placement lifecycle

**Files:** Create `crates/aq-worker/src/backend/boat/{mod,api,config,journal,policy}.rs`; modify `backend/mod.rs`, `backend/types.rs`; create `tests/boat_backend.rs`, `tests/support/boat_double.rs`.

**Interfaces:** Consumes `PlacementBackend`, `CreateRequest`, `PrivateMaterial`, `InventoryEntry`, `DestroyAuthorization`, and `conformance::BackendFixture`. Produces `BoatBackend::connect(config, key)`, `BoatConfig`, `BoatKey`, and `BackendError::Unavailable(BackendUnavailable)`.

- [x] Write an axum HTTP double with authenticated org-scoped sandbox inventory, saved snapshots, create/stop/delete, command/file transfer, and pre/post-effect 503/429/account-limit faults. Fixtures run all C1 cases; lost replies preserve discoverability. Assert failures never fabricate proof.
- [x] Run `cargo test --locked -p aq-worker --test boat_backend` through the existing C1 aq-test Cargo wrapper; expect unresolved boat module/types before implementation.
- [x] Implement requests to `POST /sandboxes` with `from`, explicit positive `ttlSeconds`, `noEnv: true`, empty env, private visibility and no hosted ports. Send `X-Boat-Org` on all operations and a deterministic `Idempotency-Key` on create. Use name prefix even if vendor labels are available.
- [x] Persist the complete CreateRequest before sending create, then bind its returned ID. Inventory uses paginated `GET /sandboxes?namePrefix=...`, validates names locally, and resolves unknown prefixed VMs as conservative orphans. Never adopt a changed boot/digest/request; never touch other prefixes. Bound every HTTP operation and redact keys/errors.
- [x] Compute sizes from CPU/memory: small <=2 CPU/4GiB, default <=4 CPU/8GiB, large <=8 CPU/16GiB; reject larger or absent limits and host mount aliases. Auto-stop is a fixed absolute hard deadline plus 5s, converted to seconds with 250ms+ RTT allowance; any update is bounded by that same absolute deadline.
- [x] Start OCI containers through fixed Docker commands, with no privilege, no socket/device/host mounts, no inbound ports, dropped capabilities and PID/CPU/memory limits. Upload private material only through the file API into a private VM directory, then copy a mode-0600 archive into the container; never place key/material in command text or env. Use the image's configured runner command.
- [x] Stop via POST stop and bounded observation until archived with a completed snapshot. Destroy only with matching retention authority, stop proof, DELETE and confirmed absence plus snapshot deletion completion. Timeout/5xx/lying success remains Unconfirmed. Resume/fork of attempt snapshots stays unsupported (warm-start deferred); advertise suspend-snapshot because archive preserves a snapshot.
- [x] Run focused boat lifecycle and conformance tests; commit implementation after checks.

### Task 2: Controller refusal and exclusive service configuration

**Files:** Modify `controller/{lifecycle,service,types}.rs`, `main.rs`; add backend selection module if needed; test in `tests/boat_backend.rs` and existing `tests/controller_cli.rs`.

**Interfaces:** Consumes `BackendError::Unavailable`; produces `RefusalReason::BackendUnavailable`. Existing `Controller<B,P>` remains generic. `RunConfig` accepts exactly one of docker_local or boat.

- [x] Add `boat_unavailable_refuses_offers_without_orphans`, exercising actual Controller reserve/accept/launch and sweep through the HTTP double for 503, 429, capacity and lost create responses. Assert typed refusal and no running VM after recovery; retained disks stay preserved.
- [x] Add `sweep_ignores_sandboxes_without_node_prefix` with another node, an unprefixed sandbox, and an owned unknown sandbox; assert only the owned prefix is stopped.
- [x] Add typed mapping for create/material/start unavailable errors; retain create intent and capacity until confirmed stop. Add configuration validation for mutually exclusive backends and exact advertised allowlisted digests. Use an enum adapter for one selected backend in main.
- [x] Run the two exact workspace filtered commands and the boat/controller area tests; commit.

### Task 3: Scanned snapshot materializations and evidence

**Files:** Modify `packaging/sandbox/template_tool.py`, `credential_scan.py`, `crates/aq-worker/src/template.rs`; add `packaging/sandbox/boat_materialize.py` and focused Python tests under `scripts/tests`; create `docs/implementation/C7.md`.

**Interfaces:** Consumes the C3b allowlist entry and credential scanner. Produces `entry.materializations.boat` with saved snapshot name/reference, immutable snapshot ID, OCI digest and scan/build provenance. No template key is used by the attempt backend.

- [x] Test scanner rejection of credential-bearing VM snapshot paths/content and materialization digest mismatches before admission. Assert atomic allowlist publication only after OCI digest verification and saved snapshot scan.
- [x] Build in a separate org/key: create no-env base VM with bounded TTL, pull the exact OCI reference and verify RepoDigests, create a stopped keeper container so boat retains the image, save snapshot and poll readiness. Scan actual snapshot data using C3b path/content checks before recording materialization; clean up build VM even on failure. Keep credentials only in the HTTP Authorization header.
- [x] Document official API facts with URLs, including asynchronous operations, scoped lifecycle/exec/file/snapshot permissions, org scope, plan limits, snapshot and resume timing, firewall rules needing reapplication, 250ms RTT assumptions and absence of a boat terminal-latency claim. Distinguish documentation from measured qualification.
- [x] Record CROSS-REPO FOLLOW-UP (aq-server) for aq-worker-boat Compose/enrollment; do not edit other repos.
- [ ] Run targeted Rust/Python checks, formatter and clippy; record known unrelated baseline limitations. Run ignored live conformance only when approved env key/config exists; lack of it is an unmet live acceptance, never a pass.
- [ ] Settle checklist, preserve one final feature commit on the recorded source base, merge origin/main, rerun targeted checks for conflicts, and publish with guarded aq git push. Close truthfully with evidence or report a blocking operator decision and preserve the held claim.

## Execution status

Local implementation and named/area checks pass (see `docs/implementation/C7.md`). Final publication/integration is in progress. Required real Boat conformance remains unrun pending approved node-local credentials/config and scanned saved snapshot; C7 is not complete and the held claim is preserved. Supervisor prerequisite request was sent once.
