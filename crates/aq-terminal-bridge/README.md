# Guarded terminal bridge, stage 1

This experimental finite control component uses an explicitly installed, already
Running `aq-session-host`0.1.1. It has no launch/resume, arbitrary command, socket
selection, renderer, snapshot, capture or unknown-resolution API. The worker
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
