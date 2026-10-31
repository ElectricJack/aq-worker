# WRK-14 stage1 integration qualification

Integrated source `f36534f04dde348264c842252c65d675b6f74ee3` combines the qualified
finite terminal component with the existing observation/graph workspace. Its
306-file build/protocol inventory is retained in
`evidence/WRK-14-integration/source.json`. Ninety of92 frozen stage1 files remain
identical; only Cargo.toml/Cargo.lock changed additively. The full Git source tree
is independently bound in that inventory. No implementation changed during this
qualification, and this evidence commit does not merge worker main.

Results:

- Root's completed full workspace/all-feature/locked/offline run passed288 tests
  with29 opt-in entries ignored. Root directly observed session56682 exit0. The
  original shell/thread count and build-job count were not retained and remain
  unrecorded. Reported test-process durations sum to138.47s; Cargo reported29.05s
  build time. These are not a measured total wall time or performance result.
- Strict workspace/all-target/all-feature Clippy with `-D warnings` passed.
- Workspace formatting verification passed.
- Both explicitly local ignored `local_relay` tests passed using actual synthetic
  tmux and a fake loopback coordinator against the integrated dependencies with
  two test threads: lost-ACK/restart repeats metadata while one
  physical Unicode line and resize are observed; a stalled receipt HTTP request
  does not hold explicit guardian execution renewal or proven stop.

Direct checks used `CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target`,
`CARGO_INCREMENTAL=0` and build jobs3 where compilation was requested. Source
hashes were rechecked after all runs. Exact logs, commands and environment are
retained in `evidence/WRK-14-integration`, with an additional external copy at
`/home/jkern/dev/.aq-qualification/wrk14-integration-2026-10-31`.

This remains an experimental finite control-only component. There is no worker
scheduler adoption or terminal capability advertisement. Capture, renderer state,
snapshot/live barriers, epochs/long-lived retention and central HTTP authority
remain outside qualification. Applied denotes bounded tmux command acceptance,
not provider/model semantic acknowledgement or a general saturated-PTY drain
proof. The23 earlier host tmux cases remain retained stage1 evidence; they were
not rerun by this integration checkpoint. No500-agent performance or physical
storage/memory envelope is claimed. WRK-14 remains incomplete; no release pin,
publication or deployment follows from these results.
