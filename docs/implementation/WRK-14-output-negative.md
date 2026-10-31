# WRK-14 bounded tail: negative qualification checkpoint

Source: `14354e7cf852f3696bfbc3e7ca4f26036e1e1de9`, isolated branch
`task/wrk14-terminal-output`, following design `9c7d225`. This is an explicitly
unqualified output experiment, not producer/runtime adoption. Host candidate
0.1.2 and bridge candidate0.1.1 leave the published identity/host observation
wire shapes unchanged. Prior qualified control-only evidence remains separate.

The candidate adds strict octal control-stream decoding, a one-shot read-only
observer, immutable physical epoch/monotonic frame sequences, sealed lower
frame/replay bounds, exact eviction gaps, and a separately reserved outbound
HTTP upload lane. Every tail begins Incomplete. Pause/exit/malformed data loses
continuity; there is no snapshot, renderer, automatic reattach, unknown-input
resolution or runtime capability claim. Loss can strand output until a separately
qualified producer/baseline or authorized new attempt.

## Results and failed acceptance

The frozen normal workspace run passed301 tests with33 ignored across77 test
processes. Two inventories retain the exact measured bytes. Subsequent changes
were diagnostic ignored-test code and synthetic flood instrumentation: stage
labels, receipt/process diagnostics, preservation of failed fixtures, and removal
of fsync from volatile progress markers. Normal runtime implementation did not
change. The final candidate passed strict workspace all-target/all-feature
Clippy and formatting. This does not convert the output failures into a pass.

Actual tmux3.4 suite runs twice produced25passes/1failure: the stopped sole
observer sometimes blocked harness progress beyond the unchanged5second bound.
An unchanged backtrace suite reproduction passed26/26, and focused runs also
passed. Those passing points are retained, not a reliability claim. A later
suite passed the stalled-reader case but timed out in the pre-existing caller
crash-boundary fixture waiting for `fault.marker`; exact cause is unresolved.

Focused diagnostics conclusively reproduced the output blocker after progress
markers became volatile: the input receipt was Applied, the fixture-start marker
existed, and `/proc/<pane>/syscall` showed `write(1, ...,8192)` while progress
remained zero after5seconds. Dropping the observer released progress. Thus the
failure cannot be dismissed as an unconsumed input or progress-marker fsync.
`pause-after=1` alone is insufficient. The separate execution-expiry case passed
while the client stayed stopped, yielding writer ECHILD proof; bounded stopping
and continued harness progress are distinct properties.

The final actual bridge run passed both existing control/HTTP cases and failed
the new tail case with `control event count bound` once the volatile fixture
produced faster output. This is conservative fail-closed behavior, but it also
fails useful output acceptance. Do not widen bounds without designing bounded
incremental processing and preserving byte/event limits.

Earlier Unicode fixture failures are retained: PTY ONLCR converted fixture CRLF
to CRCRLF. The fixture now writes LF and the test expects actual CRLF. The decoder
was not changed to normalize or invent bytes. All actual host logs, passing and
failing, are retained under `evidence/wrk14-output-negative`.

## Actual host serialization evidence

After source commit, a clean checkout generated `host-proof-pair.json` with
`AQ_HOST_COMPAT_EVIDENCE_DIR=/tmp/wrk14-host-proof-14354e7`. Its SHA-256 is
`5243730b2d148128267f52c30f6d420ee4c4449baf222c5f2ceb469357da8514`.
The exact Running record comes from the caller's durable `record_started`
callback; StoppedProof is the actual guardian result, with matching full guardian
and pane, ECHILD true, root status present and positive reap count. The actual
Unicode/alternate-byte case passed. This is narrow serialization/linkage evidence
for an independent DTO consumer, not output qualification. The earlier WIP pair
remains untouched at `/tmp/wrk14-host-proof-compat` and is not attributed to this
commit.

## Environment, bounds and next gate

Tests ran on shared Linux WSL2 x86_64 with tmux3.4,24 available logical CPUs,
`--test-threads=2`, and concurrent unrelated Cargo/tests. Environment/load samples
are retained; no isolated performance,500-agent, CPU quota or hard scheduling
claim is made. Original failed runs are not hidden by retries or longer timeouts.
The normal suite commands use locked/offline dependencies. Explicit actual
commands require only local synthetic tmux/HTTP fixtures, never providers.

Before acceptance, an independent observer strategy must prevent a stalled or
dead acquisition consumer from indefinitely backpressuring the pane, prove
continued progress and independent expiry/stop, and expose a discontinuity with
no implicit reattach/epoch reset. A guardian-enforced exact read-client deadline
is a candidate to review, not implemented here. Baseline, renderer semantics,
resync/handoff and output runtime adoption remain later gates.

`WRK-14-output-negative.json` hashes all32 retained evidence artifacts. The final
source inventory contains476 hashes. The raw logs/inventories distinguish each
measured source state; no successful full-suite claim is made for this candidate.
