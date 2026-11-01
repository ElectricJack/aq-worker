# Two-stage static capsule preparation and commit

Status: host-only synthetic implementation and static checks complete; independent
implementation review pending. Root approved this bounded increment after the
source-first design review. This document does not authorize a production join.
Production eligibility stays false. No real SourcePin, provider credential,
Codex, tmux, permission/permit bridge or production wire is introduced.

## Boundary and unchanged ownership

The accepted static capsule presently transfers the listener to its caller
after the executable is observed. Real scoped auth needs the listener's exact
origin before deriving auth/config/argv. Move the parent-visible return point
to a trusted **pre-exec pause**, then require one exact finalization commit.

Keep the accepted clean helper/subreaper, checked FD root/pivot, enforced
submount-free grants, private namespaces, IPC filter, FD closure, process
witnesses, original deadline and reaper uncertainty rules. The caller still
authorizes mount grants. Only the derived-auth directory is visible inside;
the original material/store/worker proc and source descriptors are not inputs.
The existing one-stage static profile remains distinct; the new profile is
`linux-x86_64-prepared-static-capsule-candidate-v1`, with no missing-commit
fallback to the one-stage behavior.

The parent retains physical ownership before any pause is returned. A prepared
process is a trusted helper stub, not an observed running harness. No prepared
or commit acknowledgement can become an old Started, permit or runtime
qualification receipt.

## Exact candidate API

```rust
// Existing shared ArtifactRef/Digest and ProcessEvidence; metadata only.
pub struct CandidatePreparedInfo {
    // Read-only getters; caller cannot mutate the retained identity.
    owner_intent: ArtifactRef,
    preparation_digest: Digest,
    capsule_id: Uuid,
    original_deadline_boot_ms: u64,
    guardian: ProcessEvidence,
    namespace_init: ProcessEvidence,
    exec_stub: ProcessEvidence,
    origin: String, // exact http://127.0.0.1:nonzero-port
}
pub struct CandidatePreparedHandle { /* sole owner; no Clone/Serialize/Debug */ }
pub struct CandidatePreparedStopReport {
    // Exact prepared identities + actual ECHILD; no fabricated running image.
}
impl CandidatePreparedHandle {
    pub fn prepare(
        helper: &PinnedHelper,
        plan: StaticCapsulePlan,
        owner_intent: ArtifactRef,
    ) -> Result<Self>;
    pub fn info(&self) -> &CandidatePreparedInfo;
    pub fn take_listener(&mut self) -> Result<std::net::TcpListener>; // once
    pub fn commit(self, commit: CandidateCommit) -> Result<CapsuleHandle>;
    pub fn abort(self) -> Result<CandidatePreparedStopReport>;
}

struct FilePin {
    // Private pins for fixed members, not caller-controlled filesystem routes.
    device: u64,
    inode: u64,
    bytes: u64,
    digest: Digest,
}
pub struct CandidatePublishedInputs {
    auth: FilePin, // Fixed auth.json
    config: FilePin, // Fixed candidate-config.json
    capability_digest: Digest, // never capability bytes in metadata
}
pub struct CandidateStaticRecipe { /* closed static fixture argv/env recipe */ }
pub struct CandidateCommit { /* immutable bounded metadata; no raw key */ }
impl CandidateCommit {
    pub fn new(
        prepared: &CandidatePreparedInfo,
        published: CandidatePublishedInputs,
        recipe: CandidateStaticRecipe,
        transport_scope_digest: Digest,
    ) -> Result<Self>;
    pub fn digest(&self) -> &Digest;
}
```

`CandidatePreparedInfo` binds the owner-intent ref, exact static plan digest,
nonce, original deadline and actual listener origin. The initial plan fixes
the image, exact stdin, mount grants, preparation profile and static argument
prefix. Final recipe construction may add only the profile's closed derived
origin/intent/deadline arguments and exact environment. It cannot replace the
program, prefix, stdin, mounts, PATH or parent-private variables. This is a
static fixture recipe, not arbitrary command registration or Codex support.
Commit requires that the prepared listener has been taken exactly once; this
orders the handoff but does not assert that an external proxy is live. Proxy
liveness and capability production remain the parent owner's responsibility.

`CandidateCommit` contains those preparation fields, exact auth/config pins,
capability digest, transport-scope digest, canonical recipe digest and complete
bounded recipe metadata. Its canonical digest excludes its own digest field.
Use fixed members beneath `/auth`, maximum two files, each <=64KiB, regular,
owner-private mode 0600, owner UID equal to the qualified helper, link count 1
and no symlinks. No SourcePin, original key digest-to-byte reconstruction,
parent FD route or credential-bearing argv/environment is accepted.

The initial derived directory is fresh and empty for this synthetic profile.
The parent publishes files through its retained directory owner after prepare.
It uses exclusive/atomic publication and the declared fsync protocol before
sending the commit. Namespace readonly mounts alone do not stop a trusted
outside publisher from modifying files; the publication owner must fence
further writes. Helper checks cannot be advertised as a general immutable-file
guarantee without that ownership. Real publication/history reuse is a later
production bridge, not a second secret store in this candidate.

## Prepare, publish, commit and execute

1. Parent selects an immutable owner-intent ref and a bounded original
   BOOTTIME deadline before starting physical setup. The candidate can bind
   metadata; it does not prove that an external database/filesystem fsynced it.
2. Trusted setup establishes the accepted private views and binds the listener
   inside the isolated network namespace. The outer guardian independently
   validates its FD, post-send credentials, namespace/ancestry and init/stub
   pidfds. The trusted exec stub waits; no untrusted program runs.
3. Guardian transfers the listener and `CandidatePreparedInfo` to the parent.
   Parent consumes that exact listener, prepares its scoped transport, obtains
   the fresh local capability, builds derived auth/config and the closed argv
   recipe using the exact origin, and publishes them under the same intent.
4. Parent creates and sends exactly one `CandidateCommit`. Guardian verifies
   the prepared identity, nonce, profile, origin, unchanged deadline and closed
   metadata, then forwards the exact commit to the still-paused trusted stub.
5. Stub independently opens only fixed guest `/auth` members with no-follow
   descriptor operations. It rechecks mode/UID/link count, actual inode/length/
   complete digest, local-capability digest, config intent/origin/deadline and
   recipe relationship. It authenticates the commit and checks the original
   deadline again before exec. Guardian owns caller-liveness checks before
   forwarding and during image observation, and closes the inner socket after
   forwarding; there is no later independent stub/caller liveness handshake.
   All setup/control/source FDs close before exec, as in the accepted primitive.
6. Only then does it exec the pinned static image. Guardian independently
   observes the real image/incarnation before returning `CapsuleHandle`.

First synthetic auth uses the existing closed local-capability encoding in an
`auth.json` fixture. Config is an explicitly **candidate-only** closed JSON
record of owner intent, preparation, origin, capability digest and deadline;
it is not claimed to be a Codex config. Reuse/refactor the existing host pure
auth-format checker where useful; never reuse its SourcePin-reading path from
inside the capsule. A later Codex profile must qualify its real config mapping.

Duplicate, conflicting, missing, late or malformed commits are terminal for
the preparation. No reconfiguration or repair-in-place occurs. Abort/control
loss/original expiry first fences the parent transport, then kills/reaps the
owned capsule. Exact repeated delivery is not permission to execute twice.
The Rust owner is consumed by commit; there is no commit retry API. A commit
error can be execution-uncertain if release was sent before a later failure;
it must never imply "did not run" or authorize a new attempt.

An abort before any commit has its own prepared-stop type carrying the exact
prepared identities and actual ECHILD. It must not fabricate a running image
witness to construct the existing candidate running-stop report. An absent
exec observation after commit is not proof that exec never happened. Unknown
cleanup remains owned through the accepted reaper mechanism and publishes no
stopped proof or auth/workspace release.

Neither acknowledgement nor file publication extends the original deadline.
If a nanosecond transport deadline is used, enforce the earlier of the
original owner deadline and the prepared millisecond deadline converted by
checked multiplication. Floor conversion may shorten authority; it must not
round it forward or mint a fresh duration after prepare.

## Immutable intent and production compatibility

The prepared candidate accepts a real caller-supplied `ArtifactRef` for a
synthetic owner intent and derives a separate preparation digest. Finalization
is a child commit bound to that immutable ref. This demonstrates binding, not
durable authority, and does not require fake project/task/session identifiers.

The frozen `LaunchIntent.configuration_digest` currently describes a complete
sealed configuration. It cannot silently become a template/preparation hash
because the listener origin was not known at initial setup. Production must
explicitly version its preparation/finalization profile and append the exact
commit through the existing launch intent/history, under the existing owner.
It must recheck original InstalledMaterial/SourcePin **before the clean helper
transition** and fence the final handoff without transferring original key
memory/FDs into the capsule. No production wire, journal or cleanup contract
is changed by this candidate, and no second secret journal is proposed.

## Reusing the scoped Responses transport

The current worker transport binds its own parent-netns listener in
`start_inner`. That must become an owned-listener input. Do not copy its HTTP,
JSON-policy, secret, streaming or cancellation implementation into the daemon
or host module.

Recommended independent extraction: same-workspace `aq-responses-transport`,
using existing dependencies only. It contains the current closed endpoint,
framing, JSON/item validation, per-response cap injection, bounded SSE/back-
pressure, deadline and cancellation core. It has no worker, admission, host,
material-store or daemon dependency. A neutral opaque owner-scope digest and
shared ModelSettings replace the core's remote LaunchBinding requirement.
This metadata grants no execution or credential authority.

```rust
// Proposed extraction seam, not implemented by this design.
ResponsesPolicy::new(owner_scope: Digest, model: &ModelSettings,
                     original_deadline_boot_ns: u64) -> Result<ResponsesPolicy>
ScopedResponsesTransport::start_on_listener(
    listener: std::net::TcpListener, // consumed, numeric loopback/listening verified
    expected_origin: &str,
    policy: ResponsesPolicy,
    endpoint: UpstreamEndpoint, // existing fixed HTTPS OpenAI or explicit test peer
    credential: Arc<dyn RetainedUpstreamCredential>,
) -> Result<ScopedResponsesTransport> // async; fresh local capability generated here

trait RetainedUpstreamCredential: Send + Sync {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>>;
}
```

The credential callback is narrowly for an already-authorized retained source.
Read it initially, retain the original upstream key only outside the capsule,
and recheck before every forwarded request as today. Execute the bounded
regular-file read through the existing spawn_blocking pattern, with authority
checks before and after. A canceled serving task never forwards a later read
result. A started blocking filesystem read itself may outlive task cancellation;
do not claim it was forcibly canceled. Keys remain zeroizing parent-only data.

Worker wrappers keep the exact LaunchBinding/manifest/model/permission/material
receipt and original-record joins, existing public API and production gates.
The legacy bind path can create a parent listener and call the same core; the
new private path consumes the prepared child-netns listener. Preserve old
request-policy/stream/refusal goldens and negative forwarding tests. Central
may eventually supply its real neutral scope after its own authority bridge;
it must not construct a fake remote LaunchBinding or import all of aq-worker.

Expose nonsecret transport metadata: exact scope digest, origin, original
deadline and local-capability digest. Parent commit pins these together with
the published auth/config and recipe. Host helper compares those declared
pins and actual derived bytes; it does not acquire the upstream source or
claim it independently qualified provider semantics. Per-response output cap
remains separate from aggregate cost/token budgets, which remain refused.

The parent runtime owner must own the prepared/running capsule **and** transport
and synchronously cancel forwarding before physical abort/stop, including any
commit failure or dropped preparation. Taking a listener is not cancellation
integration. Reuse the current worker physical-entry stop fence/transport owner
when the production join is designed; do not claim the host-only handle owns
an externally taken proxy task. The first static increment owns no proxy task;
an explicit aggregate RAII owner and cancellation evidence are required in the
later synthetic transport composition.

## Small implementation sequence and acceptance cases

1. After Root review, implement only the host prepared/commit candidate and a
   static peer/synthetic publication. No transport copy and no production join.
2. Independently extract the existing transport core with unchanged worker
   wrappers and owned-listener entrypoint; preserve full policy behavior.
3. Compose the two with a finite loopback synthetic Responses peer and fake
   upstream material reader. No paid/external request, real SourcePin or Codex.

Required static evidence: no guest execution/request before commit; successful
matching auth/config/argv digest then actual exec and ECHILD; missing commit;
expired original deadline; changed intent/nonce/origin/deadline; conflicting
commit; modified/symlinked/wrong-mode derived file; control loss during pause;
and parent transport cancellation on failure/drop. Retain accepted FD-root,
submount, ancestor/proc/FD, listener, IPC and process-witness checks. Positive
commit is one execution only. Negative cases forward zero upstream requests
and publish no running or stopped receipt without its actual evidence.

Independent source review precedes importing the next implementation. All
current production, real credential, named-image, tmux/PTY, permit, recovery
and packaging qualification gates remain separate.

## First implementation evidence

The host-only implementation adds a private optional prepared mode to the
test-support helper's plan. Legacy plans omit that field and retain the old
one-stage release, listener and image-witness path. The only change outside
the candidate module is extracting the existing pure local-capability auth
format/digest checker; the original SourcePin verifier still calls it after
its original scope checks.

The fixed synthetic publisher creates both files exclusively through the
retained directory FD, syncs each file and then the directory, and returns
exact file pins. The guest stub checks the actual readonly mounted directory
and files before exec. This is a trusted publisher ownership/fencing contract,
not a filesystem snapshot or protection against a malicious trusted parent.
The owner-intent ref and transport-scope digest are caller-supplied metadata;
this increment has neither a durable authority owner nor a serving transport.

Local Linux static evidence (no Codex, tmux, SourcePin or provider request):

- All 11 capsule integration cases passed in 8.21 seconds, including the
  existing one-stage process, FD-root/submount and Drop/deadline cases.
- The final five prepared cases passed in 5.63 seconds after adding the
  conflicting owner-intent vector. They cover pause without marker/request,
  exact commit and actual image exec, prepared abort/ECHILD, Drop/control loss,
  original expiry/late commit, correctly rehashed changed intent/nonce/origin/
  deadline/closed recipe, and actual auth/config byte, mode, symlink and extra
  member changes. Rejected cases verify init/stub pidfd exit and no guest
  marker; refusal alone is not advertised as a stopped receipt.
- Host unit tests passed (36 passed, three separately qualified fixtures
  ignored); the two new pure tests verify private publication, absence of
  capability bytes from commit metadata, and semantic conflict refusal.

The commit API consumes the prepared handle and has no retry method. Raw
duplicate-commit injection, external transport cancellation, real publication
history/recovery, Codex helper compatibility and tmux integration are not
qualified by these static cases. Post-release errors remain uncertain.
