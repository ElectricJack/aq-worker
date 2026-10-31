# WRK-14 stage 1: guarded input and resize

This candidate extends an existing verified guarded host. It cannot launch or
replace a session, choose a socket, grant execution, or renew execution. The
existing `LaunchIntent`, `GuardianIdentity`, `HostObservation` and caller-owned
durable `LaunchAuthority::record_started` contract remain unchanged.

The portable relay contract is AQ21 `aq-terminal-relay-protocol` 0.1.0 from
agent-q checkpoint `c8b7e2452db7a7b1c3106e59c22d5ac5ae916042`, over the frozen
`aq-session-protocol` 0.1.0 identities. Its archive is packaged without a lockfile,
extracted unchanged, and tested in a consumer workspace with the frozen shared
identity archive. Consumer dependency patches do not rewrite published manifests.

## Ownership

Work is isolated on `task/wrk14-terminal-bridge`. Owned production paths are the
new `crates/aq-terminal-bridge`, `crates/aq-session-host/src/terminal_io.rs` and
narrow host lib/supervisor hooks. Cargo/lock edits are additive. WRK15 observation
crates, reader/http implementations and its fixtures remain separately owned.

## Durable authority and effects

An immutable local terminal binding joins the exact shared terminal identity to
the existing guardian nonce, machine boot/pid namespace and exact tmux server/pane
process evidence. The terminal's session must equal the sealed launch session.
Binding is explicit, happens only for an already Running host and cannot replace
a pane under the same attempt. Reconnect recovers this binding; it never starts a
missing tmux server. Every tmux client uses `-N` and the private sealed socket.

The dedicated guardian serializes controller fences, literal input and resize.
Stop, execution permit and wall expiry are evaluated before terminal effects.
Network callers never directly issue tmux commands. Current controller generation,
lease identity, immutable operation digest, operation deadline and exact pane
identity are checked again immediately before effects. Controller expiry never
extends the independent execution permit.

The worker clock adapter requires an explicit bounded clock-skew assumption. It
projects the wall-clock controller/command deadline to a conservative boot-clock
deadline, subtracting the configured uncertainty. A wall clock regression cannot
extend an already issued boot-clock permit. An unqualified clock refuses control.

Before an input can reach tmux, its operation ID/digest and Injecting state must be
fsynced. An exact retry reads its durable receipt. Changed bytes under an existing
ID refuse. A crash, command timeout, missing guardian result or uncertain physical
effect produces DeliveryUnknown and never automatic reinjection. Unknown blocks
later input until a separately qualified fresh-capture resolution exists. Stage1
does not provide that resolution and therefore fails closed.

Input payloads must not appear in Debug/errors/logs or durable plaintext. A
private per-launch key protects the bounded request mailbox using random-nonce
AEAD with exact binding/operation context. The durable receipt ledger contains
digests and outcomes only. Missing keys, changed ciphertext or changed binding
refuse. Files use owner-only modes, no-follow/nonblocking regular-file checks,
bounded reads and fsync+directory-fsync publication. Filesystem/OS stalls remain
a deployment qualification concern; no in-process timeout can make a stuck
filesystem operation complete.

The local admission lock is nonblocking and owner-PID scoped. The guardian skips
a busy mailbox and returns to its expiry loop; it does not wait behind network
work. Queue capacity is finite. No receipt ID is silently deleted into reusable
absence. Initial finite mode refuses capacity exhaustion honestly; normalization
and epoch retirement must be proved separately before unlimited-session claims.

## Outbound bridge

The bridge consumes only the pinned typed relay envelopes and verifies exact
node/boot/session/terminal epoch. Registration requires an explicit verified-host
binding. Control receipts and fence acknowledgements have reserved queue capacity
and use the metadata-only control-upload path. Poll, output and network retry work
run outside mutable guardian/journal operation locks; the lifetime process owner
flock remains an ownership fence. A lost network reply can replay metadata,
never an Injecting/Applied/Unknown input effect. No inbound network listener,
SSH fallback or arbitrary terminal command forwarding is introduced.

## Output limitation

`SessionHandle::capture_text` remains development text inspection. It is not a
renderer snapshot, alternate-screen state or snapshot/live sequence barrier.
Stage1 must not advertise snapshot/capture delivery or fabricate a shared
`Snapshot` from this helper. Genuine terminal-state acquisition and ordering,
alternate-screen/Unicode renderer state and snapshot/live races remain required
for full WRK14 acceptance. This candidate is a bounded input/resize component,
not completed terminal viewing or composed four-harness qualification.

## Qualification plan

Pure and fake-coordinator tests cover same-ID changed bytes, exact retries,
controller takeover/revocation, stale node boot/pane evidence, expiry, dropped
acknowledgements on both sides of the effect boundary, Injecting crash and
Unknown refusal without duplicate input. Restart must reload durable receipts
before accepting another command. Ciphertext corruption and missing-key cases
must fail closed.

Actual local tmux3.4 synthetic interactive tests cover literal Unicode bytes,
resize, independent expiry/cancellation and process ownership. Slow/full network
queues must not hold mutable operation locks, block PTY handling or consume
reserved control capacity. Tests explicitly retain uncertain outcomes rather
than claiming physical non-delivery. No live provider or human terminal is used.

## Implemented candidate and remaining limits

Changed host version0.1.1 preserves the original qualified0.1.0 archive. New bridge
version0.1.0 is not adopted by the worker0.3.3 scheduler or product capability.
The guardian retains its original directoryfd and publishes a private dev/inode
anchor. Terminal bindings include that identity; handles and guardian refuse
namespace replacement. A guardian-local ledger revision highwater refuses valid
old-ciphertext rollback before another effect. Losing the physical guardian never
implicitly restarts its writer or clears Unknown.

The bridge retains its private canonical directoryfd and exclusive lock inode,
binds those identities in its checksummed document, uses relative openat/renameat/
unlinkat and freshly verifies backing bytes/membership before and after writes.
Explicit `create` and strict `open` are separate. Missing history does not silently
initialize; persistence/ownership uncertainty permanently poisons the live object.

Bridge bounds:32 terminals,1024 total retained operation IDs,64 recovery/upload
items,768KiB admission metadata and2MiB encoded journal with final-receipt/upload
headroom. Recovery rotates a derived cursor past busy terminals. Host bounds:
1024 input and1024 control records,2MiB encoded protected state, one input/resize
mailbox plus a separate fence mailbox. Byte refusal can precede row refusal. No
IDs are pruned. Finite InputV1 is supported; epochs, capture, renderer state,
snapshot/live barriers and unlimited attach remain unqualified/unimplemented.

Received/Publishing/Submitted distinguish lost preparation from an acknowledged
local mailbox. Recovery only cancels an authoritatively unstarted input and never
resubmits bytes. Stable registration identity is durable before HTTP exposure.
The outbound client has independent one-poll/one-control lanes, fixed metadata
control-upload, bounded typed responses and HTTPS production policy. Loopback is
explicit test-only; redirects, ambient proxies and automatic retries are disabled.

Costs remain bounded history work: synchronous state cloning, serialization,
fresh backing verification and fsync. Input uses at most two bounded tmux clients,
resize one; idle guardians check two mailbox names and spawn no polling clients.
Command total bounds are250ms clamped by authority deadlines. No500-agent latency,
steady-state memory/disk envelope or stuck-kernel/filesystem recovery is claimed.

## Frozen public dependencies

Evidence: `/home/jkern/dev/.aq-qualification/aq21-public-packages-2026-10-31`.
Four exact archives passed38 tests, strict workspace/all-target Clippy and format
using only archives plus original published companion fixtures, no daemon source:

- shared session0.1.0: `479778d8ea76f0605b9a20460e0bd3fda73b699337b1e80783ada0d291a68090`;
- portable relay0.1.0: `866a2c287e6bcb265e572caf7c4f6c6733caddc6481afc748b5fbba51f1c1de7`;
- pure core DTO0.1.0: `56b7e3655ea302c73351527fa24590102e0a44040a743b9880e544d22f6156c5`;
- first canonical session-api0.1.0 archive from clean original revision
  `80e66ec8edc6d83f0ec2077cd301afe14a65ecb6`:
  `a00bfa8a48d76b7c924b5c31ace5474e597792907d91b4d6eb8813c12a9c4c11`.

Original API publication tests require separately published session-api/agent-
access fixtures and the shared archive. Bare extraction failed11 missing-fixture
tests; that log is retained before successful original-companion qualification.
No package source/normalized manifest changed. The discarded current-VCS API
repackage is not claimed as a historical pin.

Scoped tests cover12 bridge fake-peer cases,4 actual loopback HTTP cases and
actual tmux control cases. Two tmux+HTTP cases prove lost-ACK restart retries only
metadata, one physical Unicode input, resize, and an indefinitely stalled receipt
request cannot hold explicit execution renewal or a proven stop. Final frozen
source workspace/full tmux qualification is recorded with checkpoint evidence.

## Final exact-source result

The frozen92-file candidate based on worker
`34d3d6aa285d4f790ea3775a21cef2ace822886f` passed212 workspace tests with29 normal
opt-in entries skipped, then all23 explicit local tmux cases and both explicit
tmux/HTTP cases. Those explicit runs exercise25 of the skipped entries. Strict
all-target/all-feature workspace Clippy and formatting passed. All92 source hashes
matched after qualification; no services/providers beyond disposable loopback
fixtures and private synthetic tmux sessions were used.

Source inventory SHA-256:
`fbc4f92b62a1906978db2639f17f096a943e4956c5c5c4450f2be3e92d14d0ba`.
Raw run/environment/source records are retained under
`/home/jkern/dev/.aq-qualification/wrk14-stage1-2026-10-31`.
This branch has not yet integrated newer worker main `dcb2a55`; aggregate merge
qualification remains necessary before runtime adoption or release pins.

Applied means bounded tmux paste-command acceptance on the exact verified live
pane, not provider/model semantic acknowledgement or a renderer/PTY-drain barrier.
The normal synthetic case independently observes one fixture line; saturated-PTY
delivery timing has no general proof here. Capture/barriers, alternate-screen
renderer state, epochs/retention, central HTTP authority, runtime source/admission
integration and a long-lived attach operating envelope remain open. This component
checkpoint does not complete WRK-14 or advertise terminal scheduling capability.
