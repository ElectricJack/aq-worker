# Native journal expiry fixtures under shared load

Task: `bold-harbor-86`.

The C1 area run recorded five deadline failures in unchanged journal tests
([original output](C1-evidence/area.log), lines 275–309). That run used Rust
1.96.1, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2` and AQ resource admission;
it finished with 219 passed, 5 failed and 30 ignored. This task uses that
recorded baseline rather than rerunning unchanged code.

## Findings

All five failure sites call live deadline checks against Linux
`CLOCK_BOOTTIME`, which advances during real HTTP, filesystem sync, private
material reads and fixture construction. Tokio paused time would not control
this clock.

| Test | Accidental timing dependency |
| --- | --- |
| `phase_and_legacy_resource_maps_share_admission_and_keep_first_exclusive_owner` | The first 5s initial grant must survive constructing and hydrating an entire second fixture before the final resource ownership assertion. |
| `phase_execution_outlives_preparation_without_retiming_historical_source_proof` | Physical hydration and prepared-input journaling must fit between the original 1s reservation cap and a 4s preparation observation; the test then waits for that observation to expire while keeping the independent 5s execution observation live. |
| `phase_input_expiry_and_foreign_observation_refuse_without_intents` | A 50ms acceptance HTTP round trip must succeed before the test can sleep 70ms and exercise the intended refusal. |
| `expired_initial_token_never_reads_or_publishes_positive` | A 100ms initial grant must survive its durable commit, producer construction and inventory acknowledgement before the intended expiry. |
| `native_result_actual_retained_owner_refuses_json_launch_and_dies_with_pipeline` | The shared physical fixture defaults to a 1s execution grant, which must survive result collection and durable channel setup. |

These are fixture lifetime hazards demonstrated by the recorded expiration
errors. The evidence does not identify which competing process or resource
caused any particular delay, or indicate that the production clock or deadline
comparisons are wrong.

## Change

`native::http::test_clock` is compiled only with `cfg(test)`. A guard freezes
native boot-clock reads for the current Tokio runtime, and explicit monotonic
advances cross existing caps. Runtime identity includes async tasks and blocking
storage jobs; separate test runtimes remain independent. Dropping the guard
removes the override. Production builds retain the existing kernel clock path.

The five reported tests opt in, including both protocol expansions of the shared
result-owner fixture. The fsync-expiry and presence-read-expiry scenarios also
opt in so they prove the same clock transition is visible inside admitted,
paused blocking operations. Physical HTTP, journal fsync, byte verification,
Git socket peer and retained resource ownership remain exercised.

The phase input and initial presence tests refuse at the exact recorded deadline.
The initial presence test deliberately waits 150ms after obtaining its original
100ms grant, proving elapsed fixture wall time cannot expire its logical test
authority. The preparation/execution test still crosses the original source
proof cap and verifies historical timestamps and replay caps are preserved.
No wire TTL, quota, production deadline check or existing ignore annotation is
changed. Tests without a guard continue to use the real kernel clock.

## Verification

Commands run through `aq test` using the retained
[Cargo bridge](bold-harbor-86-evidence/test_cargo.py). The bridge sets
`CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`, invokes the exact JSON command
from `BOLD_HARBOR_COMMAND`, uses `BOLD_HARBOR_CWD` as the checkout, retains
Cargo output at `BOLD_HARBOR_LOG`, and fails on a nonzero Cargo exit. AQ's worker cap and marker defaults are unchanged.

- Initial clock regression: passed. Initial focused run of reported scenarios
  and fsync/read companions: passed (21.45s including the AQ bridge). The first
  bridge lost successful libtest stdout through xdist; later runs retain Cargo
  output directly and include that clock test again.
- Journal area plus clock: **111 passed, 4 failed, 29 ignored**, 111 filtered
  out, 149.57s. Exact command, individual tests and failure details are retained
  in [area-cargo.log](bold-harbor-86-evidence/area-cargo.log), with AQ admission
  and its nonzero result in [area.log](bold-harbor-86-evidence/area.log).
  All five originally reported tests and the converted fsync/read companion
  tests passed in that run.

Four additional failures remain outside the converted scenarios. The two
`changed_phase_intent_or_receipt_cannot_replace_first_history` tests in
`preparation_v2_scenarios` and `refresh_scenarios`, and refresh
`replay_before_first_persistence_retains_original_transmission_after_proposal_expiry`,
failed with `phase local observation expired`. Their fixture reservations still
use a real 400ms proposal cap. The refresh physical
`native_result_fsync_cut_never_publishes_intake_or_loses_pending_evidence` test
failed reading a missing file after an expected channel-creation error; this
shares the default 1s result execution fixture, but the log alone does not prove
expiry caused that error. These tests remain unchanged, and the area result is
not a passing check.

The final focused diagnostic run passed **15/15**, with no ignored tests,
240 filtered out, in **25.40s**. It includes the five reported scenarios, clock
isolation/cleanup and real-clock fallback, fsync/read companions, both protocol
result-owner expansions, and all four area failures rerun **unchanged**. Exact
command and complete Cargo output:
[final-focused-cargo.log](bold-harbor-86-evidence/final-focused-cargo.log).
AQ receipt: [final-focused.log](bold-harbor-86-evidence/final-focused.log).
This supports intermittent fixture timing as the explanation for the three
explicit expiry failures; the missing-file failure's cause remains unproven.
It does not establish a passing complete journal area or eliminate the remaining
real-clock fixture hazards.

`cargo fmt --all -- --check` is unavailable in this nested worktree because
Cargo resolves excluded `vendor/aq-agent-api-0.1.0` against the enclosing
checkout's workspace. The [error](bold-harbor-86-evidence/fmt-all.log) matches
the existing [C1 formatting evidence](C1-evidence/fmt-all.log).
`cargo check --locked -p aq-worker --lib` passed in 19.23s through AQ admission
without enabling test support; see [check-cargo.log](bold-harbor-86-evidence/check-cargo.log)
and [check.log](bold-harbor-86-evidence/check.log).

Direct Rustfmt checks cover every changed Rust file with edition 2024 and
`skip_children=true`; this avoids formatting unrelated modules or vendor code.

## Final branch validation

The prime's generic merge instruction names `origin/dev`, which this repository
does not have. The checkout's remote HEAD and source history identify
`origin/main` as the default. `aq git pull --branch main` refreshed the assigned
task branch, and `git merge origin/main` reported `Already up to date.` No
default branch was modified. This repository-specific mismatch was reported to
the project supervisor.

After that refresh/merge, the targeted clock and affected scenarios passed
**9/9**, with no ignored tests, 246 filtered out, in **15.07s**. Exact Cargo
command and results: [post-merge-cargo.log](bold-harbor-86-evidence/post-merge-cargo.log).
AQ receipt: [post-merge.log](bold-harbor-86-evidence/post-merge.log).
