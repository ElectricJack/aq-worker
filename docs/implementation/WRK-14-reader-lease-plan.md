# WRK-14 independent read-client lease

This proposal follows the negative candidate `14354e7` and evidence `f05bea5`.
It replaces the unsafe assumption that tmux3.4 `pause-after=1` independently
protects the harness. It does not add a renderer, snapshot, baseline gate or
runtime adoption. Public LaunchIntent/GuardianIdentity/HostObservation/
StoppedProof shapes and caller `record_started` semantics remain unchanged.

## Ownership and private handshake

The existing guardian owns the fixed read-only tmux client from spawn. A private
owner-only local SOCK_SEQPACKET listener is created inside the retained guarded
launch directory. It accepts at most one bounded connection/request per tick;
no inbound network listener exists. A request binds the exact encrypted one-shot
output claim, TerminalBinding, physical guardian/pane and sealed terminal policy.
The worker consumes its one-shot claim before requesting attachment. Missing,
changed or failed claims never reopen the same epoch. Peers must have the local
owner UID and all private directory/member identities must still validate.

Only the guardian executes the fixed tmux command; the request contains no
program, arguments, arbitrary PID, shell text or file path. Guardian-owned Child
and verified ProcessEvidence/pidfd exist before it returns any capability. The
guardian sends only the stdout read FD via SCM_RIGHTS with an exact typed ready
receipt. Its original read FD is then closed. It keeps the unused stdin FD so
control mode does not exit on EOF. It never forwards input to that FD. Failed
spawn, failed receipt/FD delivery or peer death kills the exact child and
consumes the claim. There is no spawn-then-publish orphan window. Guardian crash
uses existing ancestry/emergency-reaping behavior; it cannot grant proof merely
because a reader disappeared.

## Independent deadline and bounded work

A reader gets a fixed short boot-clock lease (candidate1000ms), clamped to the
current execution permit and immutable wall deadline. This lease has no authority
to extend either execution boundary. Guardian checks occur in its existing loop (currently a5ms sleep, plus work
and OS scheduling), with at most one bounded IPC packet and one exact process-status/pidfd
check per tick; no tmux client subprocess is used for monitoring. Existing
kernel/filesystem scheduling limitations still apply; no universal real-time
claim is implied by a numeric timeout.

The guardian kills only its own exact verified read-only child on any of:
connection EOF/error, reader lease expiry, process stop state, malformed/replayed
renewal, cancellation/execution expiry, or failed directory identity. It closes
the private channel and never reattaches. Reader cleanup must not take an
unbounded wait path. Existing guardian waitpid/reaping accounts for its child;
StoppedProof remains conservative over every descendant, including this reader.
The tmux server and pane are never signalled by a reader-only lease failure.

The worker may send a monotonically sequenced drain receipt only after its
bounded nonblocking decoder/read loop reaches EAGAIN. A data-bearing/backlogged
poll cannot renew. A quiet EAGAIN may renew, but the guardian independently
rejects a stopped client even if worker polling is quiet. A sustained backlog
that never drains therefore expires rather than renewing forever. Each renewal
sequence is consumed once; duplicates do not reset deadlines. A receipt includes
its actual local boot-clock EAGAIN timestamp; the new deadline is bounded by
that timestamp plus the reader lease, never by the later dequeue time. Queued
old receipts therefore cannot extend a dead consumer. Send cadence is bounded
(e.g. at most four per second) and nonblocking; full IPC capacity does not grant
a renewal. The guardian rejects future/nonmonotonic drain timestamps. Decoder failure,
packet/event bound or EOF closes the channel and returns explicit discontinuity.
No missing byte range is invented; the physical terminal epoch is not changed.

## Qualification and exclusions

Required actual synthetic cases before acceptance:

- Sole reader SIGSTOP: prove input application/fixture start, guardian severs the
  reader, harness produces beyond pipe capacity, and execution renewal/stop still
  work. The observer sees loss, not a fabricated pause/snapshot.
- Consumer stops reading, and separately its owning worker is SIGSTOPped or
  killed: independent guardian lease frees the pane without worker threads.
- Quiet drained renewals survive beyond one reader lease; data-only polling or
  a bounded deliberately slow consumer cannot extend an undrained lease forever.
- Registration/delivery crash boundaries cannot leave a blocking unowned client;
  changed claim, duplicate attachment and arbitrary-process requests refuse.
- Reader failure cannot signal the pane/server; execution expiry still produces
  conservative stop evidence while reader/consumer are unavailable.
- Existing guarded input/resize, renewal, process ancestry, directory replacement
  and no-replay regressions remain required.

The existing decoded event bound remains unchanged. A dense valid burst may
still fail closed; this is explicit output loss, not a reason to silently widen
caps. Bounded incremental decoding is a separate follow-up if this prevents a
useful admissible output profile. No full renderer state, Unicode cell model,
resize barrier, unknown-input resolution or complete reconnect is inferred from
literal bytes or capture_text. Future producer baseline work must preserve exact
qualified startup bytes, current grant TTL and immutable hard deadline.
