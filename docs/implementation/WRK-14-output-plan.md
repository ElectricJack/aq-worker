# WRK-14 output acquisition, renderer and adoption plan

Source base: worker `d165837`. This plan reconciles the integrated stage1 evidence
with remaining WRK-14 work. It does not close WRK-14 or advertise a runtime
capability. Shared agent-q architecture/plan ownership remains with the root
agent; public wire or daemon changes require coordinated versioned review.

## Implemented and retained

Host0.1.1 owns an attempt-private foreground tmux server and pane under the
independent expiring guardian. Stage1 has exact physical/session fencing,
literal input/resize, encrypted mailbox/receipts, Injecting-before-effect,
Unknown/no-replay, directory/revision fences and outbound control HTTP. Integrated
qualification retained288 workspace tests plus two actual synthetic tmux/fake-
HTTP cases. The earlier23 host tmux cases remain separately retained. No renderer,
snapshot/live barrier, output epoch restart, long-lived input retention, central
HTTP adapter or worker scheduling adoption follows from those results.

## Output ownership and acquisition

Use one bounded read-only tmux control-mode observer for an explicitly verified
existing terminal capability. Hardcode `-N`, the sealed private socket/session,
read-only and ignore-size flags; never accept a path or tmux command from the
network. The observer may read bytes but cannot grant execution, create a pane,
send input, resize or renew a lease. Verify exact guardian/server/pane identity,
directory anchor and deadline; stop observing on any mismatch or expiry.

The installed tmux3.4 control protocol supplies `%output` and `%extended-output`
with octal escaping. Decode those into literal bytes; do not decode each chunk
lossily as UTF-8. The parser must bound line length, decoded bytes and pending
fragments, recognize command blocks/notifications and refuse malformed or foreign
pane data. Unicode bytes, ANSI/alternate-screen sequences and split codepoints
remain unchanged for a later qualified renderer. Parser limits are explicit;
unbounded lines fail the observation stream, not the guardian.

A control observer client is a reader, not a worktree writer. An observer owned
outside the writer guardian must have bounded child admission/reaping and cannot
be used as stopped-writer proof. Server/pane ownership stays under the original
guardian. Its stdout drain and bounded queue are independent of network waits
and guardian control. Qualify tmux's `pause-after` behavior under a stopped reader;
never use `refresh-client -A ...:off`: installed documentation says that when all
clients turn output off, tmux stops reading the pane, which can block the harness.
On pause/exit/protocol loss, mark the stream incomplete and detach rather than
silently resume a supposedly complete renderer history.

## Sequence and loss semantics

TerminalIdentity.epoch remains the immutable physical epoch. Each accepted output
chunk receives a monotonically increasing sequence and digest. Byte limits are
the lower sealed TerminalPolicy limits, with explicit per-session/global frame
and metadata caps. Overflow of already sequenced retained frames yields exact
merged OutputGap ranges. Unknown missing bytes are not represented as fabricated
precise ranges or invented frames. Duplicate sequence/same bytes is readback;
changed bytes conflict.

First acquisition after an already-running harness has no complete baseline.
It is explicitly a tail with initial incompleteness, not a screen snapshot.
An observer claim is one-shot per physical binding in the initial slice: losing
the producer cannot reset its sequence and restart against the same identity.
Network reconnect may replay the surviving bounded producer buffer and gaps;
worker/producer loss remains stop-and-preserve or unsupported until a distinct
resume/baseline profile is qualified. No blind relaunch or new physical epoch is
minted to hide a discontinuity.

## Full renderer and snapshot gate

A later renderer profile needs a version/digest-pinned emulator and codec, bounded
cells/styles/graphemes, safe unsupported-control behavior and wire-size admission.
Its initial complete baseline must precede harness output through an explicit
host startup handshake, or come from a separately qualified full-state source.
The current post-start text helper cannot establish cursor/modes/scroll regions,
both screens, saved state or future stream interpretation.

One producer serializes raw bytes, resize/dimension changes and captures. A
snapshot is an atomic renderer-state cut after frame N and before N+1, containing
the exact terminal identity, dimensions, renderer artifact, snapshot/capture IDs,
digest and watermark N. Unicode width/combining/wide-cell behavior, main/alternate
screen transitions, scrolling and resize ordering require actual fixture tests.
Protocol-valid resize dimensions larger than the qualified renderer budget must
refuse explicitly or invalidate rendering; never truncate and claim completeness.
Snapshot encoding must fit the frozen protocol's actual encoded/state bounds.

A reconnect first receives a complete snapshot and only frames after its barrier.
If retained frames are missing, the consumer sees gaps and requires another
qualified complete cut. A fresh unknown-input capture is queued after uncertainty
under the current controller generation/lease and unique capture token; even a
quiet terminal needs a newly minted snapshot/capture identity. No unknown-input
resolution API is enabled by a tail-only output implementation.

## Transport and runtime boundary

All transport remains node-outbound. One multiplexed poll and separate bounded
data/control upload lanes carry existing typed peer envelopes. Output content
cannot use metadata-control capacity; a slow reader/coordinator must not hold
guardian/input, execution renewal/cancellation or mutable journal locks. No
inbound listener, SSH, proxy, implicit credential use or provider action is added.
Existing daemon routes can store frames, but human viewing remains Pending until
a real qualified snapshot exists; frames alone do not make capture functional.

Runtime adoption requires the native producer's exact sealed launch/authority
join, trusted watch authorization, lower terminal-policy bounds, qualified
baseline/renderer, current ACL delivery, stopped-proof and restart behavior.
Coordinate this gate with the native producer/root. Central sessions need their
independent authority adapter, never a fake node. No product pin/deploy follows
from a component test. Physical ENOSPC/kernel stalls and scale require separate
qualification; finite buffers alone do not prove a500-agent operating envelope.

## Next coherent slice: bounded tail acquisition

Implement a bounded control-protocol byte decoder and exact-identity read-only
observer, plus a bounded sequence/digest tail queue with exact eviction gaps.
Expose explicit InitialIncomplete/Discontinuous states and one-shot producer
ownership. Test literal bytes including octal/backslash, split UTF-8/ANSI,
malformed/oversized lines, foreign pane, overflow and immutable replay. Actual
tmux fixtures must prove Unicode/alternate-screen bytes are preserved, observer
size neutrality, stale pane/expiry refusal, server stop, output flood/slow-network
isolation and bounded observer cleanup. No snapshot or renderer state is emitted.

First-slice ownership: worker output worktree only; new host terminal-output module
and bridge output module/tests, narrow existing host capability/reaper hooks as
needed, and additive Cargo changes. Do not edit daemon/public frozen contracts.
Review the concrete acquisition proof before adding renderer/baseline changes.
After this slice, proceed to the full renderer gate rather than treating a tail
as completed terminal watch/control.


## Tail experiment: blocking result

The first read-only control observer candidate failed the required stopped-reader
qualification on actual tmux3.4. With its sole client SIGSTOPped, a synthetic
flood command had an Applied input receipt and an explicit fixture-start marker,
but the pane remained in `write(1, ...,8192)` with zero64KiB progress markers after
5seconds. Volatile markers reproduced the failure, excluding their prior fsync
cost. Dropping the observer released progress. Other runs passed; those do not
establish safety. The exact execution-expiry-while-reader-stopped case passed,
but stopping writers is a different property from their continued progress.

The published3.4 [control.c](https://raw.githubusercontent.com/tmux/tmux/3.4/control.c)
checks pause age on output/write paths; this is not an independent stall timer.
This observation is a code-informed explanation, not a proof of every failure
path. The candidate is unqualified for producer adoption. Freeze its source and
negative evidence before changing the strategy. A fix must survive stopped or
dead acquisition consumers, preserve bounded memory and literal bytes, avoid
silently resetting the one-shot epoch, and never disable pane reading globally.
A guardian-enforced read-client deadline may bound disruption and force a
visible discontinuity; its continued-progress and independent-failure behavior
must be demonstrated before acceptance. No renderer or snapshot claim follows.
