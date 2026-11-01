# WRK-17: per-harness credential capsule candidate

Status: design and kernel-only feasibility evidence, 2026-11-01. Not an
implemented host profile, permission to launch Codex, or production admission.
The existing named-readonly and sealed-memfd profiles remain image-only.

## Problem and required boundary

The joined scoped Responses transport keeps the upstream key out of child
auth/argv/environment, but this does not make that key inaccessible to a
same-UID harness. The original `SourcePin` deliberately names a worker PID and
directory FD. A harness sharing the worker filesystem and proc view can read
that source via a normal path or `/proc/<worker PID or TID>/fd/<n>` despite
0700 directories and 0600 files. Scrubbing launch metadata is useful but does
not close this boundary.

The capsule must deny harness access to original material and private node
roots, including parent process, task, root, cwd and descriptor aliases. It
must retain the selected workspace, internal Git metadata, derived local auth,
explicitly allowed folders, trusted executable/helper images and a useful
terminal. The upstream key and original material owner stay outside. There is
no incoming worker connection, mandatory container utility or additional
service. Assignment, heartbeat and event delivery continue through worker pull.

Threat scope is an untrusted harness and all its descendants, including one
which ignores its harness sandbox settings. A compromised trusted worker,
kernel, host administrator or another unrestricted same-UID host process is
outside this candidate boundary. The worker must reject unsupported kernels
or forbidden namespace setup rather than fall back to shared views.

## Selected candidate: rootless private views

Use a dedicated single-threaded helper, not `unshare` in the asynchronous
worker. It creates a single-ID user namespace mapping the current host UID
and GID, a private mount namespace with private propagation, network namespace,
PID namespace, IPC namespace and UTS namespace. No root worker, subordinate IDs, setuid
`newuidmap`, privileged service, or runtime bwrap dependency is required for
this shape. Host policy may disable rootless user namespaces; such a node is
ineligible until an independently reviewed alternative exists.

Create a fresh tmpfs root and mount only explicit roots. Never recursively
bind the host root or host `/proc`, `/dev`, `/run`, `/tmp`, `/var/lib`, `/home`
or `/etc`. A new proc mount belongs to the private PID namespace. Build a
minimal `/dev` and literal `/etc` only where required. Trusted OS image mounts
must exclude operator configuration and private service state. Readonly image
routes and their ancestors remain immutable. Give the child a safe cwd and no
old-root descriptor; detach the old root. A production implementation should
use a pinned mount plan and pivot-root style teardown, rather than infer
security from a bare `chroot` call.

Bind the exact workspace and authorized internal Git metadata with their
intended access. Bind explicit additional folders only after checking that
none aliases private node roots or original material through bind roots,
symlinks, descriptors, or a parent directory. Worktree `.git` paths and Git
alternates must resolve to the separately authorized internal metadata, not
an ambient node-wide path. Mounted shared workspaces need an independent
policy for data intentionally shared with other agents. Authorization to
mount an external folder necessarily grants the contents reachable inside it.

Mount only the derived auth/config home, not its parent launch directory or
original material directory. Close every descriptor except the exact terminal,
sealed stdin, narrowly scoped setup channel and selected executable owner;
close all setup/listener-transfer control descriptors before untrusted exec,
and mark retained executable descriptors close-on-exec. Explicit
environment construction omits worker/private-record/session metadata. Drop
all capability sets and bounding capabilities and set `NoNewPrivs` before
untrusted code. Parent namespace handles and source descriptors are never
transferred. Descendants cannot regain a view of an ancestor PID namespace
by creating their own user namespace. Exact mount-lock/remount and alternate
namespace attack cases still require qualification for the new profile.

## IPC policy: a private network namespace alone is insufficient

Network namespaces isolate IP resources and the abstract Unix socket
namespace. They do not make a pathname Unix socket inaccessible if its inode
is exposed through a mounted workspace, allowed folder or internal Git root.
Readonly/nosuid/nodev binds do not prevent that connection, and scanning a
writable workspace for sockets races future creation. The separate negative
canary below actually connects to an outer pathname listener after entering
a new network namespace. See the primary Linux
[network namespace documentation](https://www.man7.org/linux/man-pages/man7/network_namespaces.7.html)
and [Unix socket documentation](https://www.man7.org/linux/man-pages/man7/unix.7.html).

The next **candidate** is an inherited kernel syscall policy installed by
trusted setup before any untrusted exec, combined with the private filesystem,
network and IPC views. The bounded x86_64 canary tests this subset:

- New sockets permit only reviewed `AF_INET`/`AF_INET6` families; a fresh
  `AF_UNIX` socket is denied, including stream and seqpacket sockets. Unknown
  families are refused instead of assuming they obey IP namespace isolation.
  Types are closed to stream/default-or-TCP and datagram/default-or-UDP after
  stripping only CLOEXEC/NONBLOCK flags. Family alone is insufficient:
  Linux rewrites legacy `AF_INET` plus `SOCK_PACKET` to packet-family creation.
  The negative control actually creates that legacy socket while namespace
  raw capability is still held, then observes filter denial **before** the
  capability drop, distinguishing the policy from an incidental cap denial.
- Anonymous `AF_UNIX` stream/seqpacket socketpairs with protocol 0 are allowed
  for local parent/child helper IPC. Datagram socketpairs are refused: the
  negative control demonstrates retargeting one to an outer pathname service.
- `io_uring_setup`, `io_uring_enter` and `io_uring_register` are refused so
  socket creation does not escape the reviewed syscall path. Unexpected
  syscall architectures and x32 encodings are rejected; more architectures
  require their own closed qualification.
- No externally connected Unix socket, setup socket or namespace handle is
  inherited. A permitted pair can exchange descriptors only with its locally
  created peer; that is safe only if the original FD whitelist is already
  closed and no outside control endpoint can inject descriptors.
- A private IPC namespace excludes original System V shared-memory IDs.
  Original POSIX IPC filesystems are not mounted. Shared workspace files are
  intentional shared data, not a claim of preventing all communication.

On the tested kernel, stream and seqpacket pairs refuse connect and reconnect
to an outer pathname listener, including AF_UNSPEC and closed-peer/shutdown
variants. Destination-bearing sendto/sendmsg either fail or deliver only to
the original pair peer. Bind/listen can report success, so a return code alone
is insufficient: SO_ACCEPTCONN remains 0, accept fails with EINVAL and an
actual outer client is refused. This is finite kernel evidence, not a proof
against every possible socket operation on every supported kernel. Keep the
exact policy, kernel support, bypass analysis and nested helper compatibility
as review/qualification gates. The policy deliberately removes behavior;
unsupported applications must refuse rather than run with a weaker filter.
No filter is installed in the actual AQ runtime by this increment.

## Secret and local transport sequencing

The following are proposed ownership seams, not exported APIs:

1. Under the existing immutable launch intent, a trusted `CapsulePlan` pins the
   launch binding, original deadline, qualified image/environment, mount
   allowlist, terminal, exact stdin and control-channel peer. A setup child is
   physically owned before it can run. Every setup packet binds a fresh
   launch nonce and stage; no packet grants authority by itself.
2. The setup helper independently rechecks original `InstalledMaterial` and
   `SourcePin` using the existing original and record digests **before entering
   the isolated execution view**. Retained material remains with the worker.
   Recheck again at the final transition fence if preparation has intervened.
   The child must not need a source path, FD or raw key after this fence.
   Also discard all raw-key **memory** before creating any process visible
   inside the capsule: a trusted clean-helper exec, after explicit FD closure
   and a narrowly bound existing-owner handoff, is the preferred fence. Merely
   forking the worker into PID1 would copy its original key buffers into an
   inner process potentially reachable via ptrace or `/proc/1/mem`. Zeroizing
   one obvious buffer is weaker than a clean exec and must not be assumed to
   remove every allocator/library copy. The clean helper never rereads the
   source after this transition. Recheck the original in the parent transport
   before forwarding. The exact verified handoff must be designed with the
   launch owner; a freestanding receipt does not become new launch authority.
3. Inside the new network namespace, bind a numeric loopback TCP listener on
   an ephemeral port. Transfer that listener FD over the private Unix control
   channel using `SCM_RIGHTS`. The worker receives an FD bound to the child's
   network namespace without joining it. It can accept and serve connections
   on that FD while its upstream client remains in the worker network view.
   The receiver verifies the trusted post-setup peer, nonce and stage and the
   received FD's socket family/type, listening state and exact bound numeric
   loopback origin. Trusted setup transfers it before untrusted exec and
   closes its copy and all control-channel descriptors at that transition.
   Never accept a listener or readiness claim supplied by the harness itself.
4. The existing in-process Responses transport consumes this retained listener
   rather than binding its own parent loopback listener. Generate its fresh
   per-launch capability and publish derived auth/config under the existing
   durable intent. Bind the exact local origin, launch/deadline and derived
   auth digest; preserve the original material/record digests separately.
   This introduces no second secret store or lease journal. The harness can
   reach only its isolated loopback endpoint; the transport alone chooses the
   independently pinned upstream and injects the qualified per-response cap.
5. The child verifies the derived bytes, permitted mount/image view, authority
   and deadline, drops setup authority and executes. The worker rechecks its
   original material before each forwarded request as it does today. Stop or
   lost authority synchronously cancels forwarding and all outstanding IO.

Failure before readiness cancels the transport, kills the owned capsule init
and obtains physical stopped-writer proof. Derived auth cleanup remains tied
to the exact matching launch and kernel ECHILD, not a socket EOF, child exit
notification or lack of requests. Recovery never regenerates the original
key, local capability or old grant; unknown ownership remains fenced.

## tmux, identity and stop semantics

Keep tmux and the outer guardian outside the untrusted capsule. The tmux pane
starts a trusted capsule shim holding the PTY. A private PID1 supervisor owns
the inner harness, which receives the same controlling terminal after an
explicitly qualified session/process-group setup. The shim remains alive
until capsule teardown, so tmux pane PID is the shim PID, **not** the harness
PID. Attach/peek/nudge still target the outer pane; process observation names
the inner harness's outer-namespace identity separately.

Existing native observation assumes the executable is the pane process and
named image evidence pins the current namespaces. Neither claim survives
this topology automatically. A separately named capsule manifest/profile and
new observed evidence variant must bind the outer pane, capsule-init and
actual harness `ProcessEvidence`/pidfds, namespace identities and image
witness. Preserve the old profile behavior and wire meaning.

Obtain the actual outer PID from kernel-generated `SCM_CREDENTIALS` on a
post-setup message, verify its namespace membership and incarnation, and
immediately retain a pidfd. Validate a closed, stage-specific message without
accepting a caller-claimed PID. The receiver uses `SO_PASSCRED`. `SO_PEERCRED`
on a socketpair created before fork describes the creation-time peer and
cannot identify the eventual inner harness. If credentials could be
explicitly forged by still-privileged setup code, keep that code trusted and
verify the expected ancestry/namespace and actual image independently. Final
untrusted code has no capabilities.

Stop targets the retained capsule-init pidfd, first graceful if supported and
then bounded SIGKILL. Linux kills remaining processes in a PID namespace
when its init dies. The outer dedicated guardian remains the subreaper and
must reap to actual ECHILD; no procfs scan or pidfd readability substitutes
for that proof. Early pane/shim/guardian death must kill init using pinned
parent-death/liveness fences. Capsule descendants must never daemonize into
an independently surviving PID namespace. A descendant user/PID namespace
still remains underneath this owned ancestor namespace and dies with it.

The existing stop ledger must bind this topology explicitly. An old pane's
exec witness, or a capsule init exit alone, does not qualify Started/Stopped
or release cleanup. PTY job control, interactive input, terminal resize,
shell helper reexec, signal propagation, natural exit and crash/recovery
remain required future integration cases.

## Alternatives

| Approach | Benefit | Limitation |
| --- | --- | --- |
| Rootless private PID/mount/net views | Same host UID; no privileged ID broker; hides all parent routes; direct local transport FD | Linux-specific; kernel policy must permit user namespaces; requires new terminal/process witness |
| Separate real host UID per harness | Ordinary DAC and proc permissions can separate originals | Unprivileged worker cannot allocate/switch arbitrary host UIDs; privileged setup or subordinate-ID broker and filesystem ownership management require separate review; still needs network/OS view policy |
| Existing image-only profile plus private auth | Already implemented, useful image and publication checks | Same-UID original source/proc access remains; not a credential isolation solution |
| bwrap/container runtime | Useful independent qualification environment | Not selected as mandatory runtime dependency; a container around the whole worker does not separate a harness from that worker's key |

## Kernel-only evidence and remaining gates

Run `python3 tools/qualification/probe-harness-capsule.py` explicitly. It
compiles a small static C canary, uses raw syscalls, creates no agent process
and makes no external connection. Compiler/static libc/Python are opt-in
qualification tools, not runtime dependencies. The probe deliberately keeps
an original canary FD open and a second parent TID alive. Under the same host
UID it proves:

- Original canary read before isolation; raw path and parent PID/TID fd/root
  aliases inaccessible; the private proc contains only inner init/harness.
  A clean static helper exec discards the parent's random original-canary
  address space before creating inner PID1. No inherited original FD or
  parent context environment remains.
- Derived auth, an explicit allowed folder and internal Git canaries visible;
  workspace writable; auth readonly; root remount denied; no default route.
- Post-setup kernel credentials identify outer init/harness/descendant PIDs;
  parent proc `NSpid` and user/mount/PID/net namespace identities agree, and
  actual pidfds pin all three.
- Child-netns listener is transferred to parent; parent accepts a local
  request and replies without `setns` or a tunnel.
- SIGKILL to namespace PID1 exits all three pidfds and the dedicated outer
  subreaper reaches kernel ECHILD. Original material is unchanged.

This is feasibility evidence for canaries, not an adversarial isolation audit
or a production implementation. No actual `SourcePin`, tmux pane, Codex,
provider semantics, mount-allowlist races, package provenance or recovery is
qualified here. No production factory or profile is ungated. Next review
must accept the topology/seams, then a bounded Rust/helper implementation can
add exact mount/FD policy, real original/derived fences, immutable image
transition evidence, terminal ownership and crash recovery. Actual harness
qualification waits for those boundaries and independent egress/filesystem
confinement; disabling implicit plugin/login/snapshot settings alone is not
sufficient.

The first view/teardown canary intentionally retains a cooperative observation
channel in its canary process. It proves FD transfer and process mechanics,
**not** a secure final control protocol or listener join. Complementary run
`python3 tools/qualification/probe-harness-ipc.py` demonstrates the actual
pathname-socket gap, datagram retarget gap, the inherited syscall subset and
private System V IPC exclusion. In this second canary, trusted pre-exec setup
creates/transfers the TCP listener, the receiver checks the peer and FD, and
the setup control FD is closed before clean canary exec. The filter survives
both exec and a tested forked descendant. Neither canary is the composed
production capsule, and neither invokes an agent or provider. Evidence is in
[the kernel canary evidence folder](evidence/wrk17-harness-capsule/README.md).
