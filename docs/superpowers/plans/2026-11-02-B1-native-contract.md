# Native execution contract implementation plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Own and publish the delegated sandbox wire contract with executable conformance fixtures.

**Architecture:** Ownership option (a): aq-worker owns the next aq-admission release, 0.2.0, under protocol/aq-admission. Preserve frozen 0.1.11 for installed native endpoints and terminal dependencies; aq-worker pins 0.2.0 separately for sandbox contract conformance until B5/B6 install the new runtime. Rust types generate the published schema, with request-dependent pure validators and no issuance, persistence, or runtime enforcement.

**Tech Stack:** Rust 2024, serde, schemars, existing bounded identity/digest types, JSON fixtures.

**Spec:** AQ task noble-summit-37.1 (complete description), worker execution spec §7, sandbox topology §3.2/§4.1/§6. Global plan: aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-plan.md.

## Global Constraints

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Contracts:** a wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version. The task is not done until valid, invalid, replay and compatibility cases pass on both sides (ecosystem §3).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- No architecture or docs/specs edits; server issuance/enforcement, runner runtime, placement, and material-lease changes belong to other tasks.
- Use resource-controlled focused Rust checks, fmt and clippy; publish exact commands and limitations in docs/implementation/B1.md.
- Required cross-family review goes through supervisor/AQ review; do not claim self-review as cross-family coverage.

## Review Focus

1. Re-adoption responses from a prior boot or changed replay must not authorize a later boot.
2. Preparation cannot perform execution operations; neither scope accepts offers or registers nodes.
3. Deadline equality is expired; re-adoption cannot change original expiry, hard deadline, sandbox, channel or generation.
4. Unknown required capability explicitly declines even when an old peer can parse it.
5. Duplicate sandbox inventory, duplicate templates, nil identities, oversized frames, and unexpected fields fail closed.

## Task 1: Contract ownership and executable red fixtures

**Files:** create protocol/aq-admission from the existing frozen package; modify Cargo.toml, Cargo.lock, crates/aq-worker/Cargo.toml; create crates/aq-worker/tests/permit_fixture_conformance.rs and protocol/aq-admission/artifacts/sandbox-v1.fixtures.json.
**Interfaces:** worker imports `aq_admission_sandbox::sandbox`; legacy runtime keeps `aq_admission` 0.1.11.

- [x] Copy pure contract to owned 0.2.0 package, pin worker test dependency, retain wire types and frozen artifacts; move placement algorithm to fixture support.
- [x] Write `permit_fixture_conformance` against published JSON cases and new `sandbox::validate_fixture` interface; assert each required case appears and expected rejection codes match.
- [x] Run the named test through resource-controlled cargo invocation and record the unsafe-acceptance failure with an accept-all scaffold.
- [x] Commit implementation after Task 2 passes (initial plan is committed separately).

## Task 2: Delegation and node contract

**Files:** create protocol/aq-admission/src/sandbox.rs, sandbox/{permit,readoption,registration,fixtures}.rs, examples/export_sandbox.rs; add module export in src/lib.rs; register new example/test in Cargo.toml.
**Interfaces:** `DelegatedPermit::validate_for(&PermitContext, RunnerOperation, now_ms) -> Result<(), ContractError>`; `ReadoptionRequest::validate()`, `ReadoptionResponse::validate_for(&ReadoptionRequest, &[DelegatedPermit], now_ms)`; `Registration::validate()`; `negotiate(required, supported) -> CompatibilityReply`; `schema() -> serde_json::Value`.

- [x] Define opaque server-retained permit ID with immutable claims: attempt, grant generation, sandbox, node/boot, scope, issued/expiry/hard deadline, per-sandbox channel. Validation is not authentication.
- [x] Define preparation/execution operation allowlists and contextual grant-state checks, with explicit stable rejection codes.
- [x] Define bounded live-sandbox inventory and correlated operation ID/previous and new boot; response lists rebound permits and typed refusals, with complete correspondence and unchanged deadlines. Exact replay of same operation/input is idempotent; altered request/response rejects. Persistence/replay ledger belongs to B2.
- [x] Define renewal `awaiting_node_readoption` without any extension fields and strict decoding.
- [x] Define registration v2 wrapping original Register plus backend kind, capabilities, max sandboxes, optional integer currency spend ceiling, digest templates with harness versions and cold-start milliseconds/cost.
- [x] Generate schema from Rust types. Tests compare generated JSON to checked-in artifact and cover strict decoding, invalid fields and compatibility.
- [x] Run named conformance and owned contract area tests; fix until passing.

## Task 3: Publication, evidence and review

**Files:** modify protocol/README.md, create protocol/aq-admission/README.md and docs/implementation/B1.md; temporary uncommitted review draft.

- [x] Document migration, opaque permit authentication obligations, replay and deadline semantics, capability negotiation, source and fixture publication.
- [x] File matching aq-server pinned consumer/conformance work and request supervisor routing (outside assigned workspace).
- [x] Run fmt, clippy and focused tests under resource controls; record exact results and environment. Package an offline reproducible source release and record digest if packaging succeeds.
- [x] Reconcile every acceptance criterion in evidence. Submit uncommitted spec/evidence draft with aq review submit, preserve outside checkout, request required cross-family review and request cross-family review; supervisor directs author close without waiting.
- [x] Squash unpublished leaf implementation history, merge the confirmed remote base (prime names origin/dev, but repository has only main), targeted verification, guarded push and required PR. Close through the required review submission flow with explicit downstream limitations, then drain-ack.

## B6a1 phase-renewal extension (task comments, 2026-11-02)

- [x] Add failing published phase handoff and renewal fixtures before implementation.
- [x] Add projections for validated phase-v2/refresh receipts, independent runner renewal
  request/response, conservative local deadlines and post-readoption correlation.
- [x] Re-export schema, run owner tests and named worker conformance, record downstream
  B2 issuance/persistence and B6 durable adapter boundaries in evidence/review.
