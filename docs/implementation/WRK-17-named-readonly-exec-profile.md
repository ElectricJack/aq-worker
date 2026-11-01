# Named read-only native execution profile: reviewed boundary sketch

Status: design/feasibility increment. The sealed-memfd profile is unchanged and
has not qualified stock Codex tool execution. No production executor is enabled.

## Exact reason for the separate profile

Pinned Codex 0.166 initializes its self/helper paths from
`std::env::current_exe()`. An anonymous executable memfd produces a deleted
pseudo-path; stock self-helper spawning then fails. Its release build also
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

A new `NamedReadonlyExecPool` owns at most two retained image descriptors and a
bounded total declared/actual installation size. Unlike a memfd pool, it does
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
