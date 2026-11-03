# B5a: blocked dependency evidence

Task: `noble-summit-37.5`. Outcome: **blocked; not implemented**.

Inspected source baseline: `61084a6cd4648bcb4c7307ee37eeac9b418ba21b`.
This evidence is committed on `aq/noble-summit-37.5`; the task close records
the final published evidence commit. Environment: Linux, Bash, assigned AQ
worktree `slot-2`, claim epoch 1. No build or runtime qualification is claimed.

## Findings

- C1 is present: `crates/aq-worker/src/backend/mod.rs` exposes
  `PlacementBackend`; `types.rs`, `lifecycle.rs` and `fake.rs` provide scoped
  sandbox identities, lifecycle records and confirmation-based writer proofs.
- `crates/aq-worker/Cargo.toml:21` and the workspace patch pin
  `vendor/aq-admission-0.1.11`. Its `src/transport.rs:140-147` defines
  `Register` with schema version, operation ID, identity, runtime,
  capabilities and features. It rejects unknown fields and supplies none of
  B1's required backend registration, template/harness inventory, sandbox
  ceiling or spend ceiling fields.
- Searches of this pinned contract and the read-only reference checkout at
  `/home/jkern/dev/aq-platform/aq-server/crates/aq-admission` found no
  delegated-permit types. B5a requires consuming the published B1 contract;
  worker-local invented wire types would not satisfy that requirement.
- `native/runner.rs:102-105` still refuses existing state. Its journal's
  restart path deliberately requires recovery. Neither behavior was changed.
- The injected publication instruction names `origin/dev`, but that ref is
  absent; `origin/HEAD` points to `origin/main`. No alternative merge target
  was assumed.

## Checks and communication

- `rg -n 'aq-admission' Cargo.toml crates/aq-worker/Cargo.toml` confirmed
  the `0.1.11` pin.
- `sed -n '132,165p' vendor/aq-admission-0.1.11/src/transport.rs` inspected
  the registration fields.
- `rg -n -i 'delegated.?permit|spend.ceiling|max.concurrent.sandboxes|template.digests|sandbox.registration' crates vendor/aq-admission-0.1.11 /home/jkern/dev/aq-platform/aq-server/crates/aq-admission`
  returned no matches.
- `git show-ref --verify refs/remotes/origin/dev` failed: the ref is absent.
- `git symbolic-ref refs/remotes/origin/HEAD` returned
  `refs/remotes/origin/main`.
- Findings were recorded in the task comment and supervisor message
  `msg-e5196e46198e45eba77a6d0aac618c22`. On the subsequent user instruction
  to close or continue, `aq message inbox --inject --json` returned no messages.

## Unmet work and restart requirements

No implementation plan, controller code, TDD cycle or adversarial review was
completed. Neither named acceptance test was added or run:

- `cargo test --locked --workspace capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason`
- `cargo test --locked --workspace journal_records_intent_before_backend_create`

Formatting, clippy and Rust test suites were not run because this change only
records the dependency blocker. This is not evidence of passing checks.

Resume after the published B1 artifact/version and registration/permit delivery
interfaces are available, and the integration merge target is confirmed. Then
write the required TDD plan, implement the full task, run the required checks
and obtain the cross-family adversarial review. No scope is waived by this
blocked close, and no design-document changes are proposed.
