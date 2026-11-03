# B5b controller recovery evidence

Task `noble-summit-37.6`, claim epoch 1. Source base:
`9358faf1958fc1950c7266f5be2ad7820cdef82c`. The implementation plan was committed
before source changes as `1d5083c4da671a20e50a603427b4cb20b8576915` in
`docs/superpowers/plans/2026-11-02-b5b-controller-recovery.md`. The final pushed
source OID is recorded in the AQ close receipt to avoid a self-referential hash.
B5b is not marked for adversarial review and follows direct integration.

## Behavior and boundaries

The controller reconciles owned backend inventory before new service admission.
It validates physical identity, template and deadline, repairs an ambiguous
create mapping only after durable CreateIntent, and never repeats create/start.
Missing or contradictory inventory retains reservations. A confirmed stop with
zero processes releases compute; only an authorized confirmed destruction
releases disk. Unknown work holds conservative disk capacity and fences admission.

A new boot submits the published B1 re-adoption request with each live execution
permit's attempt, generation and logical sandbox. Requests are persisted before
sending, grouped by previous boot, bounded to 128 sandboxes, and reused on retry.
Successful responses resume monitoring only; preparation permits cannot become
execution authority through recovery. Transport failure or renewal expiry never
relaunches the attempt. Pending re-adoption leaves renewal-loss stopping to the
runner (B6a1); the independent local hard-deadline sweep remains a backstop.
Explicit no-current-grant/revoked responses stop and preserve the sandbox.

Cleanup reasons and the first grace deadline are journaled before effects.
Retention waits for confirmed stopped writers, so writes cannot follow a
receipt that supposedly covers the final workspace. Failed extraction/handoff
keeps the disk and retries on subsequent service ticks. A successful authenticated
workspace-and-evidence retention receipt, or operator release, is durable before
destruction. Failed deletion retries the same authorization without re-extraction.
Stopped handoff is recovered before new-boot registration. Orphan cleanup has
the same preservation rule. No timeout or deadline authorizes deletion.

The backend hard deadline is the grant hard deadline plus a configured margin.
The sweep only stops owned sandboxes; foreign-node inventory is untouched.
Authenticated node poll cancellations are bound to the exact attempt,
generation, manifest digest and current session. The first observation's stop
deadline survives replay and restart. Cancellation records are constant-size
per attempt; subsequent notices cannot postpone stopping. Admission and launch
effects reject cancellation and expired authority, including acceptance responses
that arrive after the deadline.

Configuration is under `node.recovery` (omission uses these defaults):

| Field | Default | Allowed |
| --- | ---: | ---: |
| `orphan_grace_ms` | 30000 | 1–300000 |
| `cancellation_grace_ms` | 5000 | 1–60000 |
| `sweep_interval_ms` | 1000 | 1–30000 |
| `deadline_margin_ms` | 5000 | 0–30000 |
| `max_pending_results` | 128 | 1–4096 |

Old format-1 snapshots remain readable: absent optional recovery fields are not
serialized into their checksum. Journal failure poisons the invocation and
preserves reservations. The fork-lock port verifies that orderly owner shutdown
releases its lock even while an unrelated child retains an inherited descriptor.

## Synthetic runtime decision

The synthetic runtime is retained **only as an explicit test harness** behind
`synthetic-runtime`. The default production library omits `runtime`, `journal`,
`driver` and `source_driver`; the normal CLI omits `run-execution`. Legacy tests
enable the feature through the existing self dev-dependency; `test-support`
also enables it. Shared source/transfer and guardian types remain available to
native execution. Production and all-target Clippy checks validate both feature
configurations; native execution is not routed to the historical harness.

## Port ledger

All controller ports run against C1 `FakeBackend` and a typed fake provider.
They exercise actual controller journal files, backend calls and resource
accounting. Names inherited from the synthetic suite do not claim real child,
HTTP, tmux or deployed sandbox qualification.

| Original `tests/execution.rs` test | Controller coverage |
| --- | --- |
| `competing_projects_share_one_durable_capacity_budget` | Existing `capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason` and `capacity_checks_spend_resources_environment_and_drain`; one node-wide ledger |
| `admission_replay_and_changed_bytes_are_distinct` | Same-named port; durable operation and capacity unchanged on replay, changed request refused |
| `exclusive_owner_and_unknown_state_format_fail_closed` | Existing `startup_rejects_second_owner_bad_version_symlink_and_missing_state` |
| `write_failures_never_acknowledge_unpersisted_admission` | Same-named port; before-write and file-sync fault injection |
| `uncertain_directory_sync_poison_requires_recovery_and_preserves_reservation` | Same-named port; post-rename uncertainty poisons until reopen |
| `lost_acceptance_and_late_response_never_revive_expired_reservation` | Same-named port with injected clock advancing during acceptance |
| `request_send_time_includes_network_delay_and_suspend` | Same-named port verifies absolute backend deadline across a clock jump; transport renewal ownership remains B6 |
| `launch_intent_crash_never_blindly_relaunches_or_releases_writers` | Same-named recovery test; lost create response and failed stop preserve reservations |
| `actual_fixture_child_completes_once_and_report_retry_is_identical` | Same-named fake-process port; stopped output is retained and retried for the same sandbox across restart |
| `stopped_slots_do_not_bypass_pending_result_storage_bounds` | Same-named port; stopped compute does not free pending-retention allowance |
| `observations_and_directive_receipts_cross_durable_boundary` | Same-named port of cancellation observation/deadline persistence; runner report protocol remains B6 |
| `unsupported_capabilities_and_forged_grants_are_rejected` | Same-named port; capability refusal and forged grant binding |
| `directive_receipts_are_bounded_and_exact_replay_survives_the_bound` | Same-named port; repeated cancellation IDs coalesce without extending the original deadline |
| `oversized_journal_is_refused_before_deserialization` | Same-named port against controller journal |
| `orderly_journal_release_does_not_wait_for_an_unrelated_fork_child` | Same-named pipe-handshake port, no sleeps |
| `profiled_tmux_launch_remains_refused_until_authority_integration` | `preparation_permit_does_not_rebind_execution_after_restart`; controller only starts the fixed runner, agent/tmux execution remains B6 |
| `nonstart_restart_preserves_known_cancellation_and_defaults_unknown_to_expired` | Same-named port; pre-create records become abandoned without backend effects and retain known cancellation |

`tests/driver.rs` coverage:

- Cancellation: `blocked_fixture_is_killed_and_reaped_on_cancellation` checks
  deadline, failed-stop capacity hold, confirmed zero processes, preserved disk.
- Grant expiry: `request_send_time_includes_network_delay_and_suspend` covers
  the controller hard-deadline backstop;
  `readoption_failure_reuses_operation_and_leaves_runner_until_deadline`
  covers the B5b side of renewal loss. Real runner renewal-loss behavior is B6a1.
- Restart reporting: `stopped_report_recovers_before_new_boot_registration_without_relaunch`
  verifies handoff precedes registration and no launch occurs.
- Lost responses/post-stop outage: `actual_fixture_child_completes_once_and_report_retry_is_identical`,
  `grace_is_durable_and_failed_destroy_is_retried_without_reextracting` and
  `unretained_sandbox_is_preserved_not_destroyed` cover identical handoff retry,
  persisted authorization, preserved output/disk, and recovery from failure.
  The original HTTP driver itself remains test-only.

Additional tests cover unknown-orphan grace/operator release, B1 refusal,
configuration bounds, old snapshots, scoped sweeps, missing inventory, final
writer proof before retention and poll-driven service ticks.

## Verification

Environment: Linux x86_64 / WSL2, Bash, Rust/Cargo 1.96.1, AQ slot-0.
`B5b-evidence/environment.json` records the exact versions, base and worktree.
All Cargo checks use the committed lockfile. `aq test` is pytest-only here, so
we reuse the committed B5a subprocess adapter under the existing resource gate:

```sh
B5A_COMMAND='<command>' \
B5A_LOG=docs/implementation/B5b-evidence/<name>.log \
B5A_RESULT=docs/implementation/B5b-evidence/<name>.json \
aq test docs/implementation/B5a-evidence/test_cargo.py -q
```

The adapter sets `CARGO_BUILD_JOBS=2`; AQ worker caps are unchanged. Each JSON
receipt records the exact command and exit code; its sibling log is full Cargo
output. Named workspace filters must match the actual controller test; unrelated
zero-match binaries do not count. The owner velocity instruction supersedes the
generic full-suite request: only named tests, the affected controller/backend/CLI
area, formatting and Clippy are run.

| Command | Result | Receipt/log basename in `B5b-evidence` |
| --- | --- | --- |
| `cargo test --locked -p aq-worker --lib controller::` | 51 passed | `controller` |
| `cargo test --locked --workspace controller_restart_reconciles_journal_against_backend_inventory` | 1 matched controller test passed | `acceptance-reconcile` |
| `cargo test --locked --workspace unretained_sandbox_is_preserved_not_destroyed` | 1 matched controller test passed | `acceptance-preserve` |
| `cargo test --locked -p aq-worker --test backend_conformance --test controller_cli` | 18 passed | `area` |
| `cargo clippy --locked -p aq-worker --all-targets -- -D warnings` | exit 0 | `clippy` |
| `cargo clippy --locked -p aq-worker --lib --bin aq-worker -- -D warnings` | exit 0, default production features | `production-clippy-final` |
| `cargo run --locked -p aq-worker --bin aq-worker -- --help` | exit 0; no `run-execution`, `run-native` present | `production-help` |
| `cargo fmt --check` | exit 0 | `fmt` |
| `git diff --cached --check` | exit 0 | `whitespace` |

No full workspace suite or real backend qualification is claimed. The two exact
acceptance runs repeat tests included in the 51-test controller area suite; the
controller and backend/CLI areas total 69 distinct passing tests.
`final-checks.json` collects the successful receipts and CLI assertion. Archived
logs normalize trailing spaces and empty lines; error and result content is preserved.

## Test-first and intermediate failures

`red.log` captures unavailable recovery module/methods and initial import/type
compile errors, not a behavioral red run. `controller-initial` passed 26 tests;
`ports-initial` passed 44; `controller-final` passed 50 before the final
stop-before-retention regression. These are intermediate evidence, superseded
by the final controller run. Initial production Clippy logs capture legacy
imports/dead code exposed by disabling the synthetic harness and two recovery
style lints. Those were corrected rather than suppressed.

## Remaining integration limits

The B5a authorized injected-provider boundary remains: B2's authenticated HTTP
registration, issuance, polling and re-adoption composition is not invented.
`PermitProvider::retain` is the runner extraction/central retention adapter
boundary; implementations must return authorization only after both workspace
and evidence are retained (or authenticated operator release). The unavailable
production provider preserves disks indefinitely. This task validates the
controller decisions and retry behavior through a fake provider, not actual
remote artifact transfer. B6 owns execution observations and renewal-loss stop;
C2/C7 own real backend qualification; H5 and installers are untouched.
No wire schema, canonical spec or default branch is edited. Existing rootless
Docker isolation claims are unchanged. No credentials are persisted in the new
grant-monitoring records or test evidence.
