# First assembled static capsule candidate

Status: bounded test-support implementation contract, approved scope only.
This is a new isolated mechanism, not a qualified production host profile.
`PRODUCTION_ELIGIBLE` stays false. No old named/memfd profile, pane identity,
observed-launch protocol, worker eligibility or package archive changes.

## Exact scope and API

Add `aq_session_host::capsule` behind `feature="test-support"` and a separate
`aq-session-capsule-candidate` binary with `required-features=["test-support"]`.
Default production artifacts do not contain this module/helper entrypoint.
The distinct candidate tag is `linux-x86_64-static-capsule-candidate-v1`.

```rust
PinnedHelper::open(path, independently_expected_artifact) -> Result<PinnedHelper>
MountGrant::open(source, fixed_target, writable) -> Result<MountGrant>
StaticCapsulePlan::new(fresh_root, program, independently_expected_artifact,
                      exact_stdin, grants, deadline_boot_ms) -> Result<StaticCapsulePlan>
CapsuleHandle::spawn(&PinnedHelper, StaticCapsulePlan) -> Result<CapsuleHandle>
handle.witness() -> &CapsuleWitness
handle.take_listener() -> Result<TcpListener> // exactly once; caller owns this FD
handle.stop() -> Result<CapsuleStopReport>    // candidate-only, actual ECHILD
```

The first static program is a sealed x86_64 ELF of at most4MiB, with no
interpreter; this deliberately excludes Codex and its helper-path compatibility.
The bounded trusted helper may use the qualified parent OS loader before
namespace construction; its source is independently digest-pinned and sealed
before clean exec. Exact stdin is bounded and sealed nonexecutable. Neither
plan nor recipe contains a SourcePin, MaterialHandoff, provider key, lease,
permit, remote session/task identity or renderer qualification. No secret
store/journal is added.

Grant targets are closed roles: workspace, derived-auth, internal Git metadata,
bounded allowed-folder indexes, and optional explicit readonly trusted OS image
roots. Source directories are opened and retained before launch. Binding uses
the retained descriptors, not a later source-path lookup. Fresh root must be
canonical, owner-private, empty and outside every granted source root.

**Grant selection is caller-trusted.** The primitive does not discover private
node roots or decide whether a folder is authorized. Granting a folder which
contains original credentials exposes those contents; directory modes and role
names do not authorize it. All inputs in this increment's tests are canaries.
Real original-material recheck, clean verified handoff, provider isolation and
authenticated mount-grant policy remain separate requirements.

## Process and descriptor ownership

The caller clean-execs the independently pinned helper with a fresh private
control socket and explicit empty environment. An async-safe pre-exec FD fence
marks all ambient descriptors close-on-exec, preserving only the intended
control socket and owned stdio. Original worker memory disappears at this exec.
The helper starts single-threaded and calls existing
`ownership::become_subreaper`; it owns no unrelated children.

This outer guardian forks namespace setup before any untrusted code. Setup
creates single-ID user, private mount, PID, network, IPC and UTS namespaces.
Private PID1 owns the harness and descendants. A fresh tmpfs root exposes only
explicit grants and a new proc mount. No old root/cwd, parent proc mount,
namespace handle or original source FD survives final exec. Capabilities are
dropped and NoNewPrivs is set. The reviewed x86_64 socket-family/type/protocol,
socketpair and io_uring filter is inherited by exec and descendants; no other
architecture is eligible for this candidate.

Trusted setup binds a numeric loopback TCP listener inside the child network
namespace and transfers it before untrusted exec. The outer guardian verifies
post-setup SCM_CREDENTIALS, nonce/stage and received FD family/type/listening
state/exact origin. It pins init/harness outer PIDs using pidfds, incarnation,
ancestry and namespace evidence, then releases final exec. Every setup/control
descriptor closes before the static program executes. Socketpair SO_PEERCRED
creation-time identity and harness-cooperative listener transfer are not used
as evidence. EOF only triggers inspection; independently observed exact sealed
program identity is required for the candidate witness.

The caller receives distinct guardian, namespace-init and actual-harness
identities. These are never reinterpreted as an old tmux pane PID or existing
HostObservation/Started proof. The first increment has owned stdio and exact
stdin, not qualified tmux/PTY job control, dashboard interaction or readiness.

## Cancellation, failure and stop

Physical ownership exists before final untrusted exec. A failed/dropped spawn
or dropped handle closes its private control channel. The dedicated guardian
treats channel loss, explicit stop or original bounded BOOTTIME expiry as a
stop fence: signal the retained init pidfd, reap all owned descendants using
existing bounded ownership helpers, and require actual kernel ECHILD. PID1
death kills its remaining descendant namespace processes. Parent-death fences
cover worker/guardian/setup failure. No stopped result is inferred from EOF,
pidfd readability, procfs absence or an untrusted packet.

Explicit `stop()` returns a new candidate-only report after the guardian's
actual ECHILD and its expected clean exit. Unexpected helper death or bounded
cleanup timeout returns uncertainty. Drop may do bounded best-effort cleanup
but never publishes a stopped proof. A listener FD already taken by the caller
is caller-owned; this primitive does not claim transport cancellation, key
revocation, durable cleanup or resource release for that external owner.

## Required evidence for this increment

Use an explicitly invoked static C peer with no agent/provider calls. Through
the Rust module and actual clean helper, prove exact stdin, selected root
visibility, known parent PID/TID/root/FD and raw canary path denial, absence of
inherited setup/source FDs and ambient environment, local proxy usability,
inherited IPC policy, independent outer process witnesses, and init teardown
through ECHILD. Include control-loss/drop and bounded pre-exec refusal cases.
Tests requiring namespace policy are explicitly opt-in, not silently skipped
into an eligibility claim. No runtime gate relaxes after these tests.

Remaining gates include real original/derived auth publication handoff,
qualified immutable named-image transition/self-helper behavior, complete mount
authorization, nested-tool compatibility, tmux/PTY/signal integration,
independent permit and durable recovery. Those require subsequent review and
separate artifacts; the canary mechanism does not imply their completion.
