# WRK-14 independent reader lease: bounded failure qualification

Source `cfe2c335fb39933ee527ea355abe1e78cfefef02` implements the reviewed
`f0e9385` plan. The separate negative experiment `14354e7`/`f05bea5` is preserved.
Host0.1.2 remains an unpublished candidate; no prior archive is replaced.
No provider, production worker, relay scheduler or central adapter adopts output.

The guardian now spawns the fixed read-only tmux client and holds its exact
pidfd before handing its stdout descriptor to the consumer over an owner-only
bounded local SOCK_SEQPACKET connection. Failed delivery or missing ACK consumes
the one-shot claim and ends the client. The guardian checks its private socket
identity, reader liveness/stop state and a1second boot-clock deadline in its
existing loop (5ms sleep plus work/scheduling). It signals only that owned reader
on reader failure. Conservative writer stop evidence includes all its children.

Only polling that reaches EAGAIN can submit a monotonic drain receipt, at most
four per second. Its original boot timestamp bounds the renewed deadline;
queued receipts cannot extend from dequeue time or revive expiry. Execution and
wall deadlines clamp the reader lease and are never extended by it. A data-only
backlog cannot renew indefinitely. Stopped reader, stopped/dead consumer, channel
loss or decoder failure severs the observer, makes continuity unavailable and
never resets the physical epoch or reattaches. This bounds disruption; it does
not preserve bytes through loss or establish a complete renderer state.

## Frozen results

- Full locked/offline, all-feature workspace:303passed,36ignored.
- Explicit actual tmux3.4 host suite:29passed,0failed,9.09s.
- Strict workspace all-target/all-feature Clippy and formatting:passed.
- Explicit actual bridge suite:two existing control/HTTP cases passed; the new
  dense-output tail case still failed with `control event count bound`.

The stopped sole-client case produced15,269,888synthetic bytes while the guardian
severed the stopped client; independent execution renewal and stop still worked.
A separate case proved execution expiry with an unavailable observer. Quiet
EAGAIN polling survived2.2seconds, then an undrained reader was independently
cut off and the harness progressed beyond4MiB without stopping. A subprocess
case exercises dead consumer, SIGSTOPped consumer, lost ready delivery and lost
ACK after descriptor delivery; all four modes free the harness and retain the
one-shot refusal. Exact physical SIGSTOP delivery is checked by pidfd; the test
accepts either observed T state or already-completed guardian cutoff, because
the guardian may act before the test reads procfs.

Unit coverage includes queued-old/future/duplicate drain refusal, execution
ceiling, bounded descriptor transfer with CLOEXEC, descriptor identity and
unknown mutation fields. Old guarded input/resize, no-replay, private-directory
identity, independent permit, guardian-crash and ancestry tests remain in the
actual suite. Final source hashes match the frozen eight-file inventory. Raw
logs include preliminary strict-lint failures corrected before the frozen run;
no test deadline was increased. The original PTY-blocking negative logs remain
separate and unchanged.

## Remaining acceptance gates

The dense valid packet failure is conservative loss, not an accepted output
profile. The next reviewed slice retains at most one bounded raw packet and
continues parsing with consumed-wire counts and at most64events per poll. It
must not renew while raw backlog remains or widen absolute line/byte bounds.
Then actual queue-overflow/draining behavior must pass again. No snapshot,
Unicode cell renderer, alternate-screen state, resize barrier, reconnect resync,
unknown-input resolution or pre-harness baseline is claimed here.

All tests use disposable local synthetic tmux/HTTP fixtures on the shared WSL2
host described in the retained environment sample, with two test threads and
concurrent unrelated work. There is no500-agent or hard real-time scheduling
claim. A numeric lease cannot defeat uninterruptible kernel/filesystem stalls.
Resource costs include the native read client, pipes/pidfd/channel and one local
listener per guardian; socket buffers are requested at8KiB each, with kernel
metadata overhead additional. Full rendered-output runtime adoption remains
unqualified.

`WRK-14-reader-lease.json` hashes17retained artifacts in
`evidence/wrk14-reader-lease`, including complete source and frozen-run inventories.
