# Guarded terminal bridge: control and incomplete output tails

The qualified stage1 control component used host0.1.1. Candidate bridge0.1.1 uses
host0.1.2 to add bounded raw tails, not complete terminal views. It has no
launch/resume, arbitrary command, socket selection, renderer, snapshot or
unknown-resolution API. The worker
scheduler does not adopt or advertise this bridge. Central identities remain
independent local identities; HTTP supports remote node credentials only.

`Bridge::create` is explicit installation. `Bridge::open` refuses missing
root/lock/journal. Retained directoryfd, exclusive lock inode and owner-bound
checksummed state protect namespace membership. Every operation freshly verifies
backing bytes; ownership/persistence uncertainty poisons the instance. Restoring
a namespace does not unpoison it. Filesystem backup/rollback is not continuation:
loss of physical guardian/history requires refusal and an independently authorized
replacement attempt.

Attach accepts a trusted exact local terminal capability. Registration is durable
before HTTP exposure and confirmed after frozen public DTO validation.
Received/Publishing/Submitted distinguish lost plaintext preparation from an
already published mailbox. Recovery observes receipts, or durably refuses an
authoritatively unstarted input under guardian admission; it never resubmits raw
input. Only fence/resize metadata can replay. Unknown survives controller change.

Input plaintext is absent from the bridge journal. The host protects mailbox and
ledger with random-nonce AEAD, purpose-separated exact-binding AAD and private
per-launch key. It fsyncs Injecting before physical input. Directoryfd/anchor and
guardian revision highwater refuse directory replacement and old-ciphertext
rollback. The guardian never implicitly restarts after losing that highwater.

Finite bounds:32 terminals;1024 total retained operation IDs per bridge;64 items
per recovery/upload;768KiB admission metadata,2MiB encoded journal with reserved
space for final receipts/upload. Host caps:1024 input and1024 control records,
2MiB encoded protected state, one input/resize and a separate fence mailbox. Byte
limits can refuse before row limits. No IDs are pruned into replayable absence.
Epoch retirement and long-lived normalized receipts remain unbuilt; this mode
does not qualify unlimited attach.

Network I/O borrows no mutable bridge/guardian operation lock. The lifetime owner
flock remains an ownership fence. HTTP has one poll and one reserved
metadata-control slot, fixed routes, bounded bodies/deadlines, no proxy, redirect
or automatic retry. Production requires HTTPS; loopback mode requires explicit
`test-support`. Execution renewal/cancellation stays independent. Metadata can be
explicitly retried using its durable exchange; raw input cannot. The embedding
must qualify clock skew before constructing `QualifiedClock`.

Resource costs remain bounded history work: synchronous state clone, serialization,
fresh backing verification and fsync. Input uses at most two bounded tmux clients,
resize one. Idle guardians check two mailbox names and spawn no polling clients.
Each command has a total250ms bound clamped by local deadlines. A stuck kernel or
filesystem cannot be repaired by an in-process timeout. No500-agent latency or
steady-state memory/disk capacity claim follows from these finite tests.

DTO dependencies are exact immutable archives; only the consumer supplies patches.
Original API publication tests also need their published companion fixtures. See
`docs/implementation/WRK-14-stage1.md` and retained public-package evidence.

The tail observer is a fixed read-only, size-neutral tmux3.4 control client. It
decodes bounded octal payloads into literal bytes without ANSI interpretation or
lossy UTF-8 conversion. Pause, exit or malformed input makes it Discontinuous.
Every initial tail is Incomplete; none can emit a Snapshot. The one-shot producer
claim lives in encrypted host metadata as well as a marker. Startup failure,
loss, expiry or pause can strand the tail until a new qualified producer/baseline
or independently authorized attempt; same-terminal reattachment/epoch reset is
not an implicit recovery path.

Tail queues honor lower sealed frame/replay bounds, retain at most1024 frames and
64 merged exact eviction gaps. Unknown missing bytes have no invented range.
`OutputTails` reserves at most32 tails and16MiB payload capacity before attachment;
metadata/allocator and tmux buffers are additional costs. HTTP has a separate
one-request output lane using the existing outbound upload route. It forbids
snapshots and control receipts; metadata control stays independently reserved.
No output payload is persisted. Renderer/baseline/resync handoff, unknown-input
resolution and production runtime adoption remain separate gates.


### Retained negative experiment

The initial source14354e7 is frozen with negative evidence f05bea5. Actual tmux3.4 tests reproduced a stopped sole observer
blocking the harness in a PTY write for more than5seconds despite
`pause-after=1`. Guarded execution expiry still stops the writers, but continued
harness progress is not qualified. Passing individual runs do not remove this
failure. The control-only checkpoint remains distinct; no output capability is
advertised. The next candidate uses guardian-owned attachment and an independent short
read lease; consumer death, stop or undrained backlog severs that reader and loses
continuity. The streaming decoder now continues a bounded dense packet across polls without
widening its64event cap or absolute line/decoded-byte bounds; retained raw wire
is at most64KiB per observer in addition to its bounded incomplete line. It cannot
renew a read lease while that raw backlog remains. This remains experimental: full output acceptance and baseline/renderer/producer
adoption are not claimed.
