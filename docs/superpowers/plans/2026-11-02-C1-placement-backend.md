# C1 Placement Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provide the shared placement contract, conservative lifecycle decisions, deterministic fake and reusable backend conformance suite.

**Architecture:** Async trait methods separate backend control operations from journal policy. Serializable records carry identity and scoped evidence; uncertainty never permits deletion. A generic conformance driver supplies fixture requests and fault injection so subsequent real backends can run identical scenarios.

**Tech Stack:** Rust 2024, existing serde/uuid/zeroize/tokio; no new dependencies.

**Spec:** AQ task calm-delta-18.1 (C1); aq-server architecture 0005 §§4–5,11; worker framework W8/W14.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; repositories build alone.
- No secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions.
- Stopped-writer proof is required before resources are released or a replacement is started.
- A sandbox with unretained work is preserved, never destroyed.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- Controller persistence/reconciliation, real backends, wire advertisements and sandbox-run are outside C1.
- Record exact checks, environment, commit and limits in `docs/implementation/C1.md`; publish through AQ.
- Prime's focused-validation policy takes precedence over generic full-suite guidance. Run worker-area tests and lint, not full workspace CI.

## Review Focus

1. Missing/stale inventory and mismatched node/boot/attempt/backend identity must not authorize destruction.
2. A stopped status without a zero-process confirmation must not release writers.
3. Retention authorization for another sandbox or an empty receipt must be refused.
4. Duplicate create with a different digest or boot must never mutate an existing sandbox.
5. Timeout/error/unknown stop or destroy responses must leave resources protected.

### Task 1: Contract, lifecycle and conformance

**Files:**
- Create `crates/aq-worker/src/backend/{mod,types,lifecycle,fake,conformance}.rs`.
- Modify `crates/aq-worker/src/lib.rs` to export backend.
- Create `crates/aq-worker/tests/backend_conformance.rs`.
- Create `docs/implementation/C1.md`.

**Interfaces:**
- Produces `PlacementBackend`: async create, deliver_private_material, start_runner, inspect, list_owned, stop, archive, resume, fork, destroy; kind/capabilities.
- `CreateRequest` carries `Ownership` (node, boot, attempt), deterministic name, exact `TemplateDigest`, required capabilities and hard deadline. `PrivateMaterial` is redacted and zeroized, delivered only through the control channel.
- `Confirmation<T>` distinguishes confirmed proof from typed `UnconfirmedReason`; `StoppedWriterProof`/`DestroyAuthorization` are bound to sandbox identity.
- `LifecycleRecord` stores intent and lifecycle phases; `reconcile(journal: Option<&LifecycleRecord>, inventory: Option<&InventoryEntry>, retention: &RetentionState, deadline: Deadline) -> ReconcileAction` returns Adopt/Stop/Preserve/Destroy or protected Hold/Ignore.
- `FakeBackend` stores deterministic inventory with fault injection; `conformance::BackendFixture` supplies a backend, request and fault hooks; generic async scenario functions run unchanged against future adapters.

- [ ] Write the five named acceptance tests plus normal create/provision/run/stop/retain/destroy, foreign identity, proof uncertainty, digest conflict and optional-operation refusal tests.
- [ ] Run `cargo test --locked -p aq-worker --test backend_conformance` through an `aq test` foreground wrapper. Expected: compile failure for missing backend API (then behavioral failures once API stubs exist).
- [ ] Implement types and async trait in `types.rs`/`mod.rs`, pure policy in `lifecycle.rs`, deterministic fake in `fake.rs`, and feature-gated generic scenarios in `conformance.rs`.
- [ ] Run the named tests without features and with `--features test-support`; assert actual test counts and no skipped acceptance cases. Expected: all pass.
- [ ] Run `cargo fmt --all -- --check`, focused worker tests and `cargo clippy --locked -p aq-worker --all-targets --all-features -- -D warnings`. Expected: pass or clearly documented pre-existing failure.
- [ ] Record evidence and commit. Run a fresh whole-branch review under executing-plans, resolve substantive findings with regression tests.
- [ ] Squash own unpublished implementation history preserving the initial base, merge origin/dev as required by prime, run targeted tests if code changed, guarded push and explicitly close/drain.
