# B5a Node Controller Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Own node identity, registration, durable reservations and sandbox lifecycle without running agents in the node process.

**Architecture:** A single-owner controller holds a private root lock and a checksummed, versioned snapshot. Every external mutation has a durable intent; ambiguity retains capacity. A generic PlacementBackend supplies sandbox effects. One PermitProvider trait owns registration and initial issuance with an associated opaque request and opaque runner payload; only published B1 types cross that seam.

**Tech Stack:** Rust, Tokio, serde, fs2, existing C1 PlacementBackend, exact aq-admission 0.2.0 pin alongside frozen native 0.1.11.

**Spec:** Task noble-summit-37.5; worker framework sections 4.1/6/8.4/16 and topology 0005 section 4. Supervisor decision msg-e374e43f61d741aa8022af17da124d39 authorizes closing on the injected seam. Production registration/issuance routes and full HTTP acceptance-to-runner integration belong to B2 follow-up. Do not guess a route or claim deployed compatibility.

## Global Constraints

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- No design/spec edits. B5b owns reconciliation, re-adoption and automatic cleanup; B6 owns the runner payload format and execution authority. This controller starts the fixed runner only, with a preparation-scoped permit.
- Verify focused/related checks under AQ resource controls. Record existing clippy baseline rather than modifying unrelated code. Publish on the assigned branch and submit cross-family AQ review before passing close; integration owns merge.

## Review Focus

1. Crash after create: journal holds intent and capacity, restart never creates again.
2. Malformed or foreign permit: no material delivery or runner start.
3. Disk write failure before a side effect: controller poisons itself and does not call backend.
4. Unknown stop/destroy: capacity stays reserved until a matching positive proof.
5. Root substitution, symlinks and version mismatch: refuse without rewriting history.

## Files and interfaces

- Create `crates/aq-worker/src/controller/{mod,types,storage,journal,tests}.rs`.
- Modify `crates/aq-worker/src/lib.rs`, `main.rs`, `Cargo.toml` and `native/runner.rs`.
- Consume `PlacementBackend`, `LifecycleRecord`, `StoppedWriterProof`, `DestroyAuthorization`, B1 `Registration`, `DelegatedPermit`, `BackendAdvertisement`, shared `LaunchManifest`.
- Produce `Controller<B,P>::open(root,node,config,backend,provider)`, `register`, `reserve`, `accept_with`, `launch`, `inspect`, `stop`, `destroy`, `inventory`, `run`.
- `PermitProvider::Request` is opaque, not serialized by the controller. `register(&Registration)` and `issue(&Request,&SandboxIdentity,&LaunchManifest)` perform authenticated control operations; issuance returns a validated DelegatedPermit plus opaque PrivateMaterial produced for B6. Never serialize a worker-local HTTP envelope.
- `accept_with` records reservation/accept intent before invoking an injected native acceptance future, retaining uncertainty. Existing NativeClient poll/proposal/accept code remains unchanged pending B2 composition.
- Config carries resource ceilings, template/harness inventory, runtime artifact and backend kind/capabilities from the actual backend. Derive exact OCI digest from manifest environment digest. Spend accounting conservatively retains cumulative estimated cold-start debits; no claim of actual billing enforcement.

### Task 1: Durable identity and capacity

- [x] Write controller tests and compile-only scaffold. `capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason` reserves one slot at limit 1 and expects `SandboxCeiling` on a second offer. Further tests cover spend, CPU/memory/disk, drain, template/harness mismatch, fresh boot IDs, second-owner lock, old-format refusal and restart holding old reservations.
- [x] Run `cargo test --locked -p aq-worker --lib controller::tests`; expected FAIL on scaffold behavior, not missing imports.
- [x] Implement locked versioned journal, exact registration, current inventory, typed refusals and reserve/accept intent. Fail closed after journal errors; new boot never treats old records as live authority.
- [x] Re-run focused command; expected all Task 1 tests PASS. Commit.

### Task 2: Backend and private-material lifecycle

- [x] Add `journal_records_intent_before_backend_create`: backend observer reads the real synced snapshot at create time. Assert full exact template and ownership. Add real FakeBackend tests for create ambiguity, stop uncertainty, authorized destroy failure/success, registration refusal, wrong/expired permit and secret-free journal. Provider checks persisted issuance intent before responding and material remains 0600 outside the journal.
- [x] Run focused command; expected new assertions FAIL against unimplemented lifecycle methods.
- [x] Implement create/issue/deliver/start/inspect/stop/destroy with explicit pre-call state transitions and matching proof checks. Opaque provider payload uses protected files; no node credentials are given to the provider's runner handoff. Do not auto-retry ambiguous side effects.
- [x] Re-run focused command; expected PASS. Commit.

### Task 3: Run entry point and evidence

- [x] Test run service shutdown and unsupported provider refusal without backend effects. Test existing native state takes the validated recovery path rather than the blanket existence guard.
- [x] Run relevant tests; expected FAIL before changes.
- [x] Wire `aq-worker run --controller-config` to the long-lived controller with existing DockerLocal backend construction. The shipped unavailable provider explicitly blocks admission until B2 installs routes; the generic service is exercised with a fake provider. Keep foundation enrollment/heartbeat. Replace native blanket existence refusal with journal open/validation and explicit recovery-required handling; never relaunch old attempts.
- [ ] Run focused controller/native tests and both exact named workspace commands, fmt and clippy. Record all real outcomes and inherited baseline failures. No full CI run per worker policy.
- [ ] Update `docs/implementation/B5a.md`, commit, sync main and push. Submit the evidence for AQ cross-family review and follow the review/close protocol.

## Execution ledger

- Plan written after inspecting B1 and C1, before source implementation. Stacked on aq/wise-cascade-63 c413cfe2; merged prerequisite HEAD ff97d558d27e18f8bdf8eb57e2376a7205d94806.
- Ruling: native HTTP composition is deferred by the supervisor; using an opaque provider request and payload preserves the contract ownership boundary. The CLI must explicitly refuse when that provider is unavailable. No end-to-end deployed execution claim.

- Behavioral red/green evidence now covers the controller and native recovery; all 17 controller and 21 native-runner tests pass. Service shutdown and unavailable-provider assertions failed before their implementation. Registration operation intent was additionally tested red before adding the persisted field. Final workspace acceptance commands and review remain pending.
