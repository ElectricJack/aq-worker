# Observed-exec server liveness follow-up

Source `31def9c`, five files/47insertions/1deletion after the separately retained
baseline checkpoint `6cd0c15`/evidence `8605400`. Host remains the unarchived0.1.4
candidate; no public observation wire changes or runtime adoption. The full
previous qualification remains attributed to its original source, not silently
relabelled as qualification of this follow-up.

Self-review found that closed baseline/release rechecked helper identity, geometry
and reader liveness but lacked an independent exact original-server live-pidfd
check. Server death normally closes helper/reader, but that indirect observation
should not authorize a race. The follow-up explicitly checks both original server
and pane incarnation/liveness before release, positive Running and caller Started,
and checks the original retained server again immediately before helper FD exec.

New unit coverage keeps the pane identity live while the original server exits;
its liveness predicate refuses. The actual negative matrix adds server death to
durability refusal, reader drop, reader SIGSTOP, helper death and callback expiry:
all six cases refuse native exec/Started and reach stopped proof.

## Checks and retained load failure

- Host normal component tests:19library +1compile-fail doctest passed;37ignored
  cases comprise36 actual-host tests and the existing output subprocess fixture.
- Actual full host suite with explicit **RUST_TEST_THREADS=4**:36passed,0failed,
  7.54s. Existing TTLs, fixture deadlines, retry behavior and production limits
  are unchanged. The test-only missed-exec case uses its existing isolated child.
- Strict host Clippy, all targets/features and warnings denied:passed2.49s.
- Workspace formatting check:passed.

The preceding default-parallel actual run **failed:28passed/8failed,6.91s**.
Its entire log and exact source diff are retained. That diff is byte-equal to the
committed follow-up, proving that the four-thread pass used the same source.
Failures included several legacy initial-launch/crash fixtures and the observed
closed-helper fixture. The new positive sealed exec, six-case refusal matrix and
missed-exec cases passed even in this failed run.

The preserved observed fixture's private guardian stderr says
`expired/invalid execution permit`, before identity publication. Its actual
permit has sequence1 and a valid1500ms interval; the launch path had already
validated it before bootstrap work. A separate preserved legacy result reports
1744ms elapsed against its1500ms permit, also failing before identity. These
provide concrete evidence of startup failing to fit the existing permit during
shared build/test load. The retained file-mtime delta is284.52ms between launch
configuration and error file; this is **not** total launch latency and does not
separate pre-spawn work, scheduling and filesystem costs.

No blanket serialization, longer TTL, relaxed expiry, readiness retry or capacity
increase was introduced. The positive result is explicitly a four-test-thread
qualification envelope; saturated-start capacity and guaranteed startup under
arbitrary host load are not claimed. Refusing to launch on an expired permit is
the intended safety behavior. The earlier WIP preidentity failure in the prior
report had no preserved fixture; this reproduction supplies a matching failure
mechanism but does not prove that lost historical fixture's exact cause.

Commands use CARGO_INCREMENTAL=0 and the same normal dedicated target:

```text
CARGO_TARGET_DIR=/tmp/wrk14-baseline-target cargo test -p aq-session-host --all-features --locked --offline
CARGO_TARGET_DIR=/tmp/wrk14-baseline-target cargo test -p aq-session-host --all-features --test local_tmux --locked --offline -- --ignored --nocapture
CARGO_TARGET_DIR=/tmp/wrk14-baseline-target RUST_TEST_THREADS=4 cargo test -p aq-session-host --all-features --test local_tmux --locked --offline -- --ignored --nocapture
CARGO_TARGET_DIR=/tmp/wrk14-baseline-target cargo clippy -p aq-session-host --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
```

`expired-before-identity/` retains only synthetic configuration/context/anchor and
private test diagnostics; no terminal key or live credential is copied. Source
inventories cover840 files excluding evidence, with five exact changed-file
hashes. The artifact manifest hashes this report, all logs, source diff and
selected diagnostics. No additional workspace/HTTP tests are claimed here: the
full351pass/43ignored workspace and three bridge cases remain the explicitly
labelled prior checkpoint. No new functionality, Snapshot/renderer qualification,
resize sequencer, reconnect/unknown resolution or real harness claim is added.
