# Named read-only native execution profile: reviewed boundary sketch

Status: design/feasibility increment. The sealed-memfd profile is unchanged and
has not qualified stock Codex tool execution. No production executor is enabled.

## Exact reason for the separate profile

Pinned Codex 0.166 initializes its self/helper paths from
`std::env::current_exe()`. An anonymous executable memfd produces a deleted
pseudo-path that is incompatible with ordinary helper-path resolution. The
observed tool flow returned ENOENT, but its exact failing exec syscall and sole
cause have not yet been established. Its release build also
refuses creating helper aliases when CODEX_HOME is under the process's temporary
directory. A stable path on a separately qualified read-only image addresses
both boundaries without copying 319 MiB for every attempt.

Do not reopen a mutable installation path as a fallback for the existing
`linux-tmux34-sealed-native-exec-stdin-v1` profile. A separately named profile,
`linux-tmux34-named-readonly-native-exec-stdin-v1`, will have a distinct installed
manifest kind, opaque preflight image owner and kernel verification rules.

## Enforcement assumption (must be independently qualified)

The named executable and required native helpers reside on a filesystem/mount
that is read-only **from the worker and every harness descendant**, at stable
absolute paths outside writable workspace/private-state mounts. They have no
writable open file descriptors, writable hard-link/mount aliases, or alternate
writable source paths visible inside that execution environment. Neither the
worker nor the harness can change those mounts, join a more privileged namespace
or regain mount administration capabilities. Host/container provisioning outside
that boundary is trusted; it must not replace the image during active owners.
A read-only bind mount alone does not establish all these properties.

Candidate kernel evidence combines:

- preflight full byte digest/size/header/architecture checks against independent
  exact artifact pins, charged once per admitted image/node;
- a retained O_RDONLY/O_NOFOLLOW descriptor and exact device/inode/size;
- `fstatvfs(ST_RDONLY)` on that descriptor, plus a `statx` mount identity and
  retained worker mount-namespace identity;
- effective/permitted privilege checks and a separately qualified environment
  that prevents acquiring a parent mount namespace or remounting a writable
  alias. Checking CapEff alone is insufficient to prove this environment;
- per-attempt O(1) descriptor/mount/namespace checks and a fresh nofollow open of
  the same named route, proving it still resolves to the exact retained inode.

Named path identity is necessary because Codex reopens that route for helpers.
The filesystem guarantee supplies immutability between the one full hash and
later opens. `ST_RDONLY` is not a content digest or a substitute for the rest of
the environment qualification. fs-verity can provide an additional independently
verified immutable-file profile; no external fs-verity utility is mandatory.

The fixture can use bubblewrap to build this environment for qualification.
Bubblewrap remains a test/provisioning tool, not a required runtime dependency
of aq-worker or aq-session-host. A production read-only worker/container image
needs its own exact environment artifact and independently verified privilege,
mount, namespace and writable-root assumptions.

## Concrete shape before broad implementation

A new `NamedReadonlyExecPool` owns at most two retained program descriptors and
two fixed auxiliary descriptors per installation (four auxiliaries maximum), all
charged to one bounded declared/actual installation size. Unlike a memfd pool, it does
not allocate another copy of those executable bytes. Preflight hashes once before
registration/poll. The host image identity records the new profile and original
mount/namespace evidence; the old sealed-image serialization and checks retain
their existing identities and limits. Default/old profiles cannot accept named
read-only evidence in place of immutable seals.

The observed native gate continues exact sealed startup stdin, original deadline,
blank-baseline observation and live kernel executable evidence. A named image's
`/proc/<pane>/exe` device/inode must match the retained verified file. A physical
Running receipt establishes that exec identity; it does not qualify model,
provider, tools, context counting or credential delivery.

Use a private CODEX_HOME under a dedicated state root outside the standard
process temporary directory, for example `/var/lib/agent-q/runtime/<launch>/`.
This permits stock helper aliases whose targets are the named read-only image.
The state root remains outside the granted worktree. The manifest/private-home
intent and ECHILD cleanup ownership rules are unchanged.

## Startup settings and independent confinement

The closed mapper now explicitly disables both shell snapshot generations and
login-shell requests, plus plugin/remote/recommended/discovery/app and memory
startup surfaces. Exact Codex 0.166 source gates curated Git/HTTP sync on the
plugins feature. These settings need actual trace validation; they are not an
independent network or filesystem boundary.

Earlier actual traces found operator profile opens by shell snapshots and a
background curated-plugin Git fetch even though the model endpoint was loopback.
The finite model-peer evidence therefore establishes provider request routing
and cap injection only, not whole-process absence of external network traffic.
Subsequent CLI qualification requires an independent network namespace and
masked operator homes before execution. A whole-fixture namespace proves the
fixture's isolation, not yet a production per-harness isolation mechanism.

For production, the harness should need only its scoped local transport. Routing
from a separately confined harness to a parent transport requires an explicit
local bridge/namespace design; it is not smuggled in as a tunnel or an implicit
mandatory external service. The production refusal remains until that boundary,
this named image profile and other declared capabilities are qualified.

## Initial typed implementation seam

The initial source provides opaque `NamedReadonlyEnvironment`,
`NamedReadonlyExecPool`/`NamedReadonlyExecImage` and bounded readonly auxiliary
owners, `NamedReadonlyObservationProfile`, and the new observed-startup constructor.
Worker `NamedExecManifest` has a separate named-readonly tag and independent
`environment` artifact pin; `NamedExecInstallation::open` compares that pin against
both its separately supplied expectation and the inspected pool. The old memfd
manifest/profile remains distinct.

Local environment inspection requires all five capability sets empty and
`no_new_privs`, retains mount/user namespace descriptors, and checks their live
identity. Image preflight refuses writable same-device mount aliases, inherited
writable descriptors, hard links, mutable ancestor routes and noncanonical paths.
It hashes program and fixed helper files once, then caches actual readonly file
owners. Revalidation checks kernel/namespace/route/mount/metadata identity without
reading executable content. The observed gate transfers and executes the retained
named descriptor; it preserves native sealed stdin and existing physical proof.

These necessary checks are not a complete environment qualification: unprivileged
namespace creation is possible even with empty capabilities. External provisioning
must independently qualify mount-locking and namespace transitions, prevent unseen
writable aliases, and freeze privileged external mutation while owners are active.
This image profile does not qualify private upstream-key visibility, model context,
provider transport, shell tools or production admission. Kernel/source validation
and the paired launch/auth join remain required before any actual CLI execution.

## Initial source and kernel checks

The source-only named profile and typed installation are implemented without
changing legacy sealed-memfd image verification. Validation so far:

- Host unit suite: 26 passed in 0.21s; three explicit process/image probes remain
  ignored. This includes the old sealed image policies and the new actual-byte,
  writable-mount and live writable-descriptor refusal cases.
- Distinct named-manifest/no-self-attestation-boolean unit: passed in 0.00s.
- Confined readonly pool probe: passed in 1.12s, checking a real pinned host ELF,
  digest refusal without budget charge, 32 cache reuses of the same owner, exact
  mount substitution refusal, noncanonical symlink refusal and entry limits.
- Confined worker installation probe: passed in 1.10s, checking independently
  expected environment mismatch, actual cached program and two fixed auxiliary
  owners, exact byte charging and 16 unchanged revalidations.
- Strict host+worker all-target/all-feature lint: passed in 12.19s.

These probes only read/hash pinned ELF inputs; none executes Codex or establishes
shell-tool, provider, private-source or model qualification. The namespace wrapper
now explicitly drops all capabilities, sets `no_new_privs` and remounts the image
parent `/opt` readonly. Private state/workspace use distinct tmpfs under masked
`/var/lib` and `/tmp`. Operator homes and host service state remain hidden and
external IP egress has no route.

The single-ID user namespace maps root-owned system tmux to an unmapped UID; the
fixture therefore uses a byte-identical owner0500 copy, retaining the original
ownership policy and digest. A read-only owner0500 file can refuse a write open
with EACCES before reaching the mount's EROFS check; the pool independently verifies
`ST_RDONLY`. Neither issue relaxed an image/environment check. Shared-target stale
host metadata required a host-source mtime rebuild to expose the new API; frozen
package archives and versions were not changed.
