# Observed launch and ordered terminal proof: concrete API proposal

Base: current worker main, preserving host0.1.3 RenderedStartup and native
consumer behavior. No host source edit is authorized by this document alone;
root review precedes implementation. Pure renderer715c4b3 plus REP correction
is independently frozen and remains an abstract profile, not a physical Snapshot
producer. Version allocation for the additive host candidate is coordinated
before changing any retained0.1.3 archive.

## Additive public surface

Keep `launch` and `launch_rendered` unchanged. Add opaque `QualifiedElf`,
`ObservedStartup`, `PendingObservedSession`, `VerifiedBlankBaseline` and
`ReleasedObservedSession` types; none has a public Deserialize or constructor
that accepts a caller-written proof. A closed `ObservationProfile` names the
exact host/tmux/helper, renderer artifact, codec/width policy and initial pane
size; unknown profiles refuse. Startup contains the original RenderedStartup,
exact TerminalIdentity and qualified executable capability, not new prompt text.

Proposed calls (names reviewable, no transport implied):

```
QualifiedElf::open_verified(path, expected_artifact, installed_policy)
ObservedStartup::new(rendered, terminal, executable, observation_profile)
SessionHost::prepare_observed(root, launch_id, startup, permit, local, adapter, authority)
    -> PendingObservedSession
pending.open_baseline_observer() -> (OutputObserver, VerifiedBlankBaseline)
pending.release_after_baseline(baseline, durable_authority) -> ReleasedObservedSession
released.poll_started(durable_authority) -> Option<SessionHandle>
```

`prepare_observed` records the existing durable launch/guardian/start intents,
then returns only after the owned pane helper is at a closed pre-exec gate. It
does not call record_started or return an active host. `open_baseline_observer`
uses the existing guardian-owned one-shot reader and returns an opaque proof only
after its attach/drain checks and the helper's gate are verified. The caller may
construct the pure renderer's explicitly assumed blank model from those proved
facts; the model library itself remains host-independent.

`release_after_baseline` requires a new `ObservedLaunchAuthority` durability
method to record exact baseline acceptance before release. It sends one fixed
exec permit and returns promptly. `poll_started` is bounded/nonblocking so the
caller can continue draining bounded output while awaiting positive exec proof;
only then does it invoke the existing record_started callback with the actual
frozen Running observation. Baseline/exec failures and dropped callers never
relaunch or imply NeverGranted. Stop-and-preserve is available through opaque
pending/released handles without manufacturing a Started receipt.

## Sealing, helper gate and positive exec evidence

Add an optional private ObservedLaunchSpec to SealedLaunch, omitted when None so
legacy configuration serialization/digests stay unchanged. It binds the planned
baseline ID, terminal identity, closed observation/profile references, requested
pane geometry and exact executable artifact/length/inode/device policy. The
existing configuration digest seals this metadata; actual baseline receipts bind
that immutable digest and never rewrite it. Host LaunchIntent/GuardianIdentity/
HostObservation/StoppedProof wire shapes remain unchanged.

The new pane helper establishes an owner-only bounded local gate with the
existing guardian. Kernel peer credentials, exact original PID/startticks,
verified server ancestry and the retained directory identities bind it. It emits
no pane bytes and does not exec before a valid release. Phase remains Starting;
no human controller effect is admitted pre-Running. Reader registration while
Starting is allowed only for this exact sealed gate/profile and one-shot claim.
The guardian holds ownership of the read client before attachment, as already
qualified; lost ready/ACK/reader/consumer invalidates the gate.

The executable capability uses a retained descriptor verified against the pinned
ELF digest and installed immutability qualification. Device/inode are recorded;
path equality alone is insufficient. The helper receives the fixed executable
FD over the authenticated local gate and uses FD-based exec, with literal sealed
argv/environment. Scripts/npm launchers are not this profile. An FD prevents
path-replacement races but does not prevent in-place rewrite; absent qualified
immutability/sealing or a fresh exact digest check, exec refuses. Any required
sealing/copy must have declared byte/memory cost and cannot silently consume an
unbounded per-attempt ELF copy.

The gate descriptor is CLOEXEC. EOF alone is not success: the guardian requires
the same live PID/startticks and `/proc/<pid>/exe` device/inode matching the held
qualified executable. A helper error/death cannot pass that test. The first
profile also requires target ELF identity distinct from the gate helper; an
identical-image exec would need a stronger independent kernel exec-event proof.
Short-lived or ambiguous exec without positive evidence is no-Started/Unknown.
This is kernel exec evidence only. Harness readiness, --no-daemon behavior,
model/tool execution ownership, sandbox policy and remote pending effects remain
separate adapter qualification; a TUI pane proves none of them.

## Real blank baseline and original authority

A fresh private tmux3.4 server/pane, controlled helper and no pre-exec bytes form
the blank premise. Attach ACK or EAGAIN alone cannot prove a baseline. Observe
actual pane dimensions and compare them with the helper's TIOCGWINSZ and the
sealed requested geometry. Window size is not automatically pane size (tmux's
status line is one reason). The observed profile fixes status/layout policy and
refuses mismatch; it never guesses rows from a window resize receipt. No prior
session/pane, shell initialization, arbitrary command hook or fallback is used.

Before release the guardian rechecks exact helper/reader identity, live read
lease, the one-shot baseline/drain receipt, cancellation, original execution
permit and immutable wall deadline. Readiness never resets either deadline.
Native runtime separately clamps to current material/use authority. The original
RenderedStartup descriptor, payload reference and literal bytes are preserved;
there is no context join, newline change, welcome text or unaccounted prompt.
The existing immediate pre-exec payload hash check remains required.

The caller reserves bounded baseline/output memory before preparation and drains
locally while execution is not yet Started. Daemon TerminalActive/registration
cannot be bypassed with a fake Started. Publish/register only after the real
record_started durability step. Loss while post-grant but never Started uses a
separately qualified no-start recovery path; current prior-Started stop reporting
and NeverGranted cannot be repurposed to release it.

## Sequencer experiment before any Snapshot producer

All output-affecting physical resizes in an observed profile must enter the same
guardian-owned fixed control-command sequencer. Legacy input/resize behavior
stays unchanged outside that explicit mode. No untrusted command string or
control stdin FD is exposed. Ordinary literal input still uses the existing
fenced durable no-replay path. Unexpected layout changes invalidate consistency.

Candidate proof to test, not assume: fixed tmux control command begin/end blocks
are ordered behind already processed output on that client. Issue only one
known command at a time. A fixed nonce display command identifies capture
barriers; resize uses only validated exact pane/window identity and dimensions.
Prove on actual3.4 that pre-command output precedes its begin boundary, command
execution cannot interleave pane processing, and post-resize bytes follow the
correct boundary. Data still in the kernel PTY but not yet processed by tmux is
not claimed as already displayed. Harness CSI resize requests/unknown layout
changes must invalidate the model instead of acquiring an invented ordinal.

If command begin/end ordering is insufficient, do not mint a Snapshot. If resize
may have executed but its completion is lost, represent Unknown, invalidate the
renderer and never blindly repeat. Frozen ResizeReceipt.applied cannot express
that richer proof; retain it only for its existing acceptance scope. Initial
pure-model clip semantics must also be compared against actual tmux resize/
reflow before claiming agreement. The baseline gate alone does not qualify it.

A new versioned proof/envelope is needed before daemon adoption: exact physical
binding and baseline ID; renderer/profile reference; event ordinal and preceding
output watermark; committed resize generation/dimensions or explicit uncertainty;
fresh capture request ID and barrier/image digest. Literal OutputFrame bytes and
all frozen0.1.0 identities remain unchanged. No synthetic resize ANSI is inserted
as harness output. Old peers refuse unknown required capabilities, not silently
ignore resize events. Root reviews schema/routes before edits in agent-q.

Captures sampled after an explicit new request may share an unchanged output
sequence when quiet. Their current physical/renderer barrier must still be
proved; a cached image/digest or watermark is not freshness. Input uncertainty
resolution retains exact current controller/lease fencing and never auto-types
again. The pure model's image method does not implement these authorizations.

## Qualification order and ownership

First qualify the closed pre-exec gate with a separate synthetic ELF, original
TTL/wall expiry, no start before durable baseline acceptance, lost FD/ACK,
consumer/guardian loss and exact successful/failed/short-lived exec identity.
Then qualify same-client output/resize/capture boundaries with recorded markers,
Unicode/alternate-screen/cursor oracles and overflow; capture_text is only a test
oracle. Finally freeze the versioned proof/renderer artifacts and qualify node
outbound relay/daemon consumers. No provider or deployment is required for these
steps. Full runtime adoption remains blocked until every applicable gate passes.

This agent owns the future host baseline/sequencer changes in a new worktree from
current main after review. Root owns native runtime/control and shared daemon
contracts. No host lib/supervisor edit will be made against the old0.1.2 branch or
overwrite the0.1.3 rendered path.
