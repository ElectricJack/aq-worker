# B1b: prepared/mail scope and acceptance-to-permit handoff (aq-admission 0.3.0)

Task `agile-delta-29`. This builds on the approved B1 contract (review
rev-keen-ember r6) without changing any 0.2.0 wire document, frozen phase type
or published fixture outcome. All additions are additive, so 0.2.0 vectors
still pass unchanged. Adding public enum variants is semver-breaking in Rust,
so the crate is released as **0.3.0**. The 0.2.0 archive stays published.

## Delivered

| Deliverable | Where |
|---|---|
| `prepared` (preparation only) and `mailbox_poll`/`mailbox_ack` (execution only) runner operations | `protocol/aq-admission/src/sandbox/permit.rs` (`RunnerOperation`, scope allowlists) |
| `PreparationAuthority` projection of the frozen `AcceptedReceipt` | `src/sandbox/handoff.rs` |
| Initial issuance `PermitIssuanceRequest`/`Response`, correlated to the frozen `AcceptRequest` | `src/sandbox/handoff.rs` |
| Grant upgrade `PermitUpgradeRequest`/`Response`, correlated to `PrepareRequest`/`ExecutionReceipt`, reusing B1's `PhaseExecutionAuthority::validate_handoff` | `src/sandbox/handoff.rs` |
| Registration-v2 enrollment `RegistrationResponse` (the PermitProvider seam for B5a) | `src/sandbox/handoff.rs` |
| Replay validators `validate_issuance_replay`, `validate_upgrade_replay`, `validate_registration_replay` | `src/sandbox/handoff.rs` |
| Schema plus fixture cases (`preparation_permit`, `permit_issuance[_replay]`, `permit_upgrade[_replay]`, `registration_enrollment`, `registration_replay`) | `src/sandbox.rs`, `src/sandbox/fixtures.rs`, `artifacts/sandbox-v1.schema.json` |
| 49 new vectors (52 → 101): valid, invalid, replay and compatibility | `artifacts/sandbox-v1.fixtures.json` |
| Immutable source release | `protocol/releases/aq-admission-0.3.0.{tar.gz,json}`, sha256 `770823e88cd6aa786e4e136cec82cabdffb5fe6f4e340d481d6b3e520744c827` |

## Scope semantics

- **Preparation** scope: fetch preparation material, observe, upload artifact,
  `prepared`. The permit is bound to the lease's node, boot, attempt and
  generation, and its hard deadline is the preparation deadline.
- **Execution** scope: everything B1 allowed, plus `mailbox_poll`/`mailbox_ack`.
  Mail is session-addressed, and no session exists before the grant. These
  operations are not the node's offer poll.
- Neither scope can accept offers or register incarnations.
- The upgrade keeps the permit ID and sandbox/channel/node binding. It requires
  the preparation permit to still authorize `prepared` at upgrade time, and
  bounds the execution permit by the grant through `validate_handoff`.

## Vector provenance

Vectors are not hand-written. The test
`sandbox_handoff_vectors_derive_from_frozen_phase_payloads`
(`tests/sandbox.rs`) builds them from the frozen phase-v2 and refresh-v1 cases
in `tests/preparation_refresh_common`. It asserts that each vector produces its
expected outcome, and that each one is published byte-for-byte in the fixtures
file. Setting `AQ_SANDBOX_HANDOFF_OUT` makes the test dump them.

## Boundaries

There is no server runtime, no HTTP route and no local wire type. B2 owns
issuance, persistence, authentication and replay storage. B2 and B6a2 consume
the 0.3.0 release through `sandbox::validate_fixture`, just as aq-worker's
`permit_fixture_conformance` does. The worker dev-dependency is now pinned to
`=0.3.0`.

## Verification

| Command | Result |
|---|---|
| `aq test docs/implementation/B1-evidence/test_cargo.py` with `B1_CARGO_COMMAND="cargo +1.96.1 test --locked -p aq-admission@0.3.0 --test sandbox"` | 1 passed; cargo reported 11 passed ([log](B1b-evidence/sandbox-tests.log)) |
| `cargo +1.96.1 clippy --locked -p aq-admission@0.3.0 --all-targets -- -D warnings` | clean |
| `cargo +1.96.1 fmt --manifest-path protocol/aq-admission/Cargo.toml --check` | clean |
| `cargo +1.96.1 test --locked -p aq-worker --test permit_fixture_conformance` | 1 passed, all 101 vectors |
| `python3 scripts/package-admission.py --check` | no drift, 120 files |
| `git diff --check` | clean |
