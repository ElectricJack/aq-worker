# Test-support ordinary native CLI gate

The test-support-only CLI accepts a numeric-loopback provider only together with
explicit local transport and independently checked existing fixture kernel /
filesystem premises. A Node loopback URL alone still refuses. The ordinary
production CLI exposes none of these transport flags. The actual configured
Codex factory/preflight and ordinary run_once remain the execution path.

A thin NativeExecutor decorator emits bounded non-secret metadata after actual
preflight, then waits for one closed continuation frame before any registration
or Poll. It delegates execute unchanged. The preflight-only pipe has a fixed30s
bound, a1024-byte frame cap and no offer/grant authority. Partial lines time out;
extra frames and unknown fields refuse. No issued execution limit is extended.

The shared confinement checker now accepts an explicit expected readonly binary
path; old actual-Codex unit fixtures keep their original worker-test path. The
new CLI requires /opt/aq-qualification/aq-worker. This does not qualify an absent
new launcher/wrapper or establish runtime eligibility by an environment flag.

Checks: six focused bin cases passed in0.02s after1.00s final rebuild; the
production-only actual CLI parser test passed in0.00s after10.21s rebuild.
Final selected strict checks passed for test-support in0.53s and default in0.38s.
The cases cover actual Clap requirements, numeric origin restrictions, closed
continuation data, bounded real pipe reads including partial-line timeout and
production flag absence. Raw logs are preserved beside this record. No Codex,
host, Git, provider, database or canonical process was launched by these checks.

This is source/CLI qualification only. Real named preflight, private listener
handoff, actual AQ owner/Graph/task integration and authenticated client/result
composition remain later physical gates under the approved composition plan.
