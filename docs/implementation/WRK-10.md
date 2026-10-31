# WRK-10 guarded tmux host evidence

Revision `d02f517ee1cdf3f9dab50b09063aeeaa4a45b7f7` implements public
`aq-session-host` 0.1.0 and the minimal shared ownership integration in worker
0.3.2. [Evidence JSON](WRK-10.json) records source hashes, artifact hashes, commands,
logs and limits. The [embedding contract](../../crates/aq-session-host/README.md)
defines authorization callbacks, expiring permits, exact stop proof and bounds.

The worker workspace passes 101 tests and strict Clippy. Its routine run retains
17 explicit ignores: the 15-entry local tmux suite, one subprocess output fixture,
and the real aq-git qualification. The local tmux suite is separately enabled and
passes all 15 entries (14 scenarios plus its helper); the real aq-git test was not
run for this increment. Formatting passes. No database/provider is needed.

The clean Cargo package was extracted with the exact archived session-protocol
0.1.0 dependency. It independently passes five unit tests and the same 15-entry
real tmux suite without a sibling repository checkout. Both package archives and
their checksums are retained under
`/home/jkern/dev/.aq-qualification/session-host-2026-10-31/`. No package was
published and no product runtime pin was changed.

Tests use tmux 3.4 and synthetic interactive children. They cover central/remote
identities, caller/guardian launch-boundary crashes, socket/pane collisions,
literal interaction, failed durable callbacks, escaped descendants, adopted-child
churn, cancellation, expiry after caller death, renewal races and no fallback
shell/relaunch. Guardian loss remains uncertain; only the exact guardian's durable
kernel ECHILD receipt yields the distinct host stop-proof type.

Review removed steady-state tmux polling processes, bounded output and child
cleanup, fenced pending renewals, and capped per-tick child processing. One local
sample measured RSS of 3988 KiB for the guardian, 6004 KiB for tmux and 3360 KiB for
the synthetic pane. This is not a deployment capacity guarantee. Memory/CPU/process
hard ceilings remain an explicit trusted environment responsibility; only caller
admission mode is qualified here.

Worker profiled launches remain explicitly refused. WRK-08 must add the durable
host evidence format, admission/recovery and typed stop-proof gates before source
checkpoint/report/resource release. AQ-17 owns central authority integration.
Actual harness configuration/credentials, terminal relay and full TUI snapshots
remain separate work. Worker 0.3.2 is component source evidence, not a replacement
for the currently pinned product worker 0.3.1.
