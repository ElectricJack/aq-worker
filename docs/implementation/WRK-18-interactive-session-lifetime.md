# Interactive session lifetime: operator limitation and required follow-up

The first interactive dashboard smoke intentionally uses the existing 120-second phase lease. It proves the visible chat path only; it is not a persistent interactive workflow. Long sessions and cancellation are required V1 parity work after that smoke, not optional scope. This note records the source trace without changing any limit, protocol, deployment or running session.

## Current two-minute behavior

There are two different deadlines:

| Value | Source and meaning |
| --- | --- |
| Original execution wall | `aq-session-protocol::ResourceLimits.wall_time_ms` is a positive `u64`, not capped at two minutes. The selected execution profile supplies it. Selection policy `max_lineage_elapsed_ms` is `u64` and permits up to 604,800,000 ms (seven days). |
| Initial phase lease | `aq-admission` 0.1.10 `src/preparation_shared.rs`, `PhasePolicy::validate`, restricts `maximum_execution_lease_ms` to 1..=120000. This shared implementation serves the frozen `preparation_v2` and `preparation_refresh_v1` public families. |
| Physical permit | `aq-session-host/src/lib.rs`, `MAX_PERMIT_MS = 120000`, is enforced by `ExecutionPermit::validate_at`. It bounds each permit, not the separately sealed original wall. |

On the daemon, `profiled_execution/phase_execution/owner_core.rs` computes `execution_deadline_ms = issued + min(manifest.resources.wall_time_ms, selection_policy.max_lineage_elapsed_ms)`, then sets grant expiry to `min(issued + maximum_execution_lease_ms, execution_deadline_ms)`. On the worker, `DurablePhaseExecution` retains both deadlines, but the current native runner stops at `grant.deadline_ns()`. The launch scope, original physical permit and scoped transport also retain that lease deadline. Configuring a one-hour wall alone therefore does not produce a one-hour current native phase session.

## Existing renewal primitives and missing join

`SessionHandle::renew` already admits monotonic, same-session permits before current expiry and refuses an extension beyond the original sealed wall or after cancellation. The worker also has `NativeClient::renew`, and the daemon has its current-authority/health/material/source-fenced renewal owner.

Those HTTP/durable owner paths are currently anchored to the **legacy initial grant**: daemon `renewal/pinned.rs` calls `initial_grant::load_original_grant_locked`, whose query requires the actual legacy `kind='prepare'`, `response.kind='grant'` receipt. Worker `NativeClient::renew` consumes legacy `Granted`. The current `DurablePhaseExecution` explicitly has no renewal interface. Its phase receipt cannot be converted or relabeled as that old grant.

A proper short-lease long session must join an actual phase renewal receipt through the retained worker journal, current launch scope, host permit, runner deadline and any launch-owned transport/material lifetime. It must preserve the original hard wall, exact grant/binding, cancellation and response-loss rules. Existing renewal algorithms and host primitives can be reused; this phase ownership join is missing.

## Smallest bounded one-hour alternative

A newly selected one-hour initial interactive lease would avoid adding a renewal loop, but requires an explicitly released/negotiated long-session profile. It must not silently widen frozen 0.1.10 families or the old host validator. The two logical caps are the phase policy bound and physical permit bound. Host validation is called at initial launch and renewal in `lib.rs`, during supervisor admission/renewal/exec in `supervisor.rs`, and immediately before native exec in `observed/exec.rs`; all applicable checks must select the new bound consistently while old profiles retain 120000.

No integer-width or SQL schema change is needed: 3,600,000 ms and 3,600,000,000,000 ns fit the existing checked `u64` arithmetic and `i64` timestamp storage. The selected execution wall, lineage allowance and retained source/runtime inputs must actually cover the hour; changing a catalog label alone is insufficient. Do not globally replace `120000`: preparation, credential retrieval, inventory and health freshness windows are different concepts. `MaterialLease`'s 120-second bound is its retrieval/install window; retained installed material is subsequently verified as the original bytes/record, not retimed by changing that lease.

The long-profile choice also changes failure behavior: a one-hour initial permit can outlive daemon connectivity for substantially longer than a short renewable permit. Decide that explicitly together with cancellation behavior. Never obtain a long session by ignoring expiry, sleeping past a deadline, resetting first-send observations, extending the admitted hard wall or weakening stop checks. Controller and input deadlines remain short, independently renewed and bound to the same actual session.
