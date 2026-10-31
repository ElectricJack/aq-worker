# Native worker runtime and first harness integration

This is the native consumer implementation lane. The existing fixture driver and
journal retain their own wire/state format; a real harness is never disguised as
the fixture. The shared execution/profile designs and AQ08 producer plan remain
normative. Components are developed independently of an already runnable daemon.
Actual HTTP/product qualification waits for the exact producer contracts.

## Contract and ownership boundaries

Consume exact retained aq-admission0.1.6 for NativeOffer, startup descriptor,
acceptance/preparation/initial grant and Started/Stopped reports. Verify its
standalone archive SHA256
`d74db09f5766a437b9d43b6d63710a56afc4db69432953ef06699ec7ab09e066`.
Use the exact Session0.1.0 and Lifecycle0.1.0 types already pinned by the worker.
Pending0.1.7 adds bounded native polling, reconciliation and running-renewal replies;
its publication must precede HTTP consumer wiring. Frozen0.1.6 PollPage cannot
silently carry NativeOffer or undocumented current-authority fields.

A small native runtime module owns local capacity/reservation, immutable request
bytes, monotonic journal phases and outgoing reports. It does not select profiles,
change source/task requirements, fabricate server qualification, or issue grants.
The existing source owner supplies immutable imported base/worktree facts; private
material installation and agent-client provisioning are separate finite owners.
The reusable tmux host owns physical processes, independent permit expiry and
stopped proofs. The worker joins these actual boundaries and journals uncertainty.

## Final startup payload

The final UTF8 payload is already rendered by AQ and sealed through exactly one
startup descriptor in manifest.modules. Verify descriptor/artifact bytes, all
ordered input slice hashes and sizes, context budget and exact retained references.
Deliver only the final bytes. Do not concatenate constituent contexts again.

Add an explicit `SessionHost::launch_rendered` path while preserving legacy
`launch` behavior and its configuration serialization. The host accepts the
trusted runtime's validated descriptor reference and payload reference/bytes,
checks exact module pin, size/hash and bounds, stages only one context member,
and seals the descriptor/payload in its private configuration digest. The pane
helper rechecks staged bytes before exec. This does not add an aq-admission
implementation dependency to the generic host. Full descriptor semantics and
server authority remain the runtime owner's responsibility.

## Durable execution flow

1. Validate exact installed runtime/harness/CLI/environment and required features,
   manifest/resource/confinement policy and available capacity. Persist a
   no-launch reservation and stable acceptance bytes before any acceptance I/O.
2. Charge the complete request/response interval against conservative delivery
   TTLs. Preserve original reservation deadlines; late replies cannot revive them.
3. Retain the exact original PreparationPlan. Fetch only assigned bounded
   artifacts and private materials through current authenticated native routes.
   Verify all digests/lengths/identities before installing owner-only files.
4. Import the actual base transfer and obtain a confined aq-git worktree. Keep
   bundle/source ownership separate from the source-admission observation artifact.
   Unknown physical effects remain reserved and protected; no implicit pruning.
5. Bind private credentials to the exact attempt/model/profile purpose. Node
   credentials stay outside agent-accessible files. Provision the AQ agent client
   through its existing allocator/issuance boundary, not invented token material.
6. Persist prepared artifact/material receipts and a stable Prepare request.
   Only a matching actual grant admits process start. Seal launch intent before
   host effects; a lost start reply is Unknown, never a new launch with another ID.
7. Start the selected pinned harness through the tmux host, with its tested
   default worktree settings and exact final startup payload. Started binds the
   actual physical guardian/pane evidence to the prior logical owner and grant.
8. Renewal uses the separately versioned running-authority reply. Preserve grant
   identity and original execution hard deadline; reject stale revisions and
   consumed TTL. Retrieval-lease expiry never invents another material delivery.
9. Cancellation/expiry use independent guardian stop. Persist actual stopped
   evidence and immutable outbound request before reporting. Keep holds when
   stop cannot be proved, including granted-but-never-Started recovery gaps.
10. After proven stopped writers, checkpoint/export/upload through actual source
    owners, reconcile pending agent effects and submit the immutable result claim.
    Recorded means retained, not eligible/completed/released. No deletion follows
    without the owning release disposition.

## Qualification and integration

Core tests exercise exact package consumption, changed identities/bytes, duplicate
and delayed replies, crash-prefix journal recovery, reservation/disk bounds,
private file symlinks/permissions, lost acceptance/prepare/start/stop/report replies,
source corruption, material expiry/rotation and original hard-deadline preservation.
Real synthetic tmux tests qualify literal payload bytes, actual guardian evidence
and stop before source handoff. These do not qualify a paid provider.

The first real CLI adapter separately pins exact CLI/version and configuration,
checks actual supported confinement/model/reasoning/context/auth interfaces and
refuses unsupported required behavior. CLI/session observability and accounting
are capability-specific. Live provider qualification needs the selected configured
target and bounded budget; it does not block ordinary local implementation.

## Implemented component checkpoints

Host source3825480 (version0.1.3) combines the exact final-payload API and guardian
reader fixes; its separate union qualification records307 normal tests,32 actual
host and3 actual bridge tests. Consumer sourcebd17969 imports the exact unchanged
aq-admission0.1.6 archive (58 files), adds bounded inline chunk assembly with exact
prefix retry and verifies the actual descriptor/payload before host construction.
Its [focused qualification](WRK-08-startup-consumer-qualification.json) records6
passing tests and strict worker lint. Inline assembly is capped at2MiB with256KiB
chunks; larger Git bundles must use the source owner.

Native journal, private material installation, current HTTP authority, actual Git
hydration and the Codex adapter remain implementation work. No launch capability
is advertised by these component checkpoints.

Source7b729d3 adds the native reporting/download HTTP client over frozen0.1.6:
registration, Started, Stopped, result recording, artifact ranges and private
material. [Qualification](WRK-08-native-http-qualification.json) records6 actual
loopback HTTP peer tests plus6 startup tests, strict worker lint and formatting.
Two bounded reporting connections remain independent of two bounded download/
result connections. Replies must match original request and owner pins; material
TTL charges the full CLOCK_BOOTTIME send/body interval. The client never retries,
follows redirects or interprets a historical acknowledgment as runnable authority.

The [Codex adapter plan](WRK-08-codex-adapter-plan.md) now pins local CLI interface
observation evidence and explicitly requires standalone guardian-owned executor
qualification, exact stdin payload delivery and observed readiness before any
real runtime capability claim.
