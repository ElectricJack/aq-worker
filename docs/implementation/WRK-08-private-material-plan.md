# Native private material installation

The installer is a separate local owner between verified HTTP delivery and the
native Prepare request. It does not issue a grant, infer a running process, or
copy node/supervisor credentials into an agent workspace. Only the exact material
leases in the opaque accepted preparation can be installed.

A configured existing private root is opened component by component without
following symlinks. The root and lease directories require the effective worker
UID and mode0700; regular material/record files require mode0600 and one hard
link. An exclusive directory lock admits one installer owner. All member access
is relative to retained directory descriptors, with fixed UUID-derived names.
The configured quota counts pending and committed lease directories, at most1024,
with at most64KiB material plus16KiB canonical nonsecret record per lease. Unknown
members, partial installations and full capacity refuse further installation;
there is no recursive cleanup or unbounded recovery scan.

The nonsecret record pins exact lease, original preparation digest, material-fetch operation, logical worker
boot, actual kernel boot, CLOCK_BOOTTIME send/receive/material-fsync readings and
the conservative original retrieval deadline. `private_binding` is
`aq.private-material/{lease_id}@1`, with a digest of that canonical record.
The public MaterialReceipt uses the server delivery clock's server_time_ms as
its received_at_ms provenance watermark. This is not a synchronized receive
instant. Actual local timing remains in the private record. No secret bytes,
paths, tokens or copied environment are included in the public receipt.

Installation writes and fsyncs the secret, then the canonical record, then its
private staging directory. A no-replace rename publishes that directory, followed
by root-directory fsync. The original preparation and retrieval deadlines are
checked again after these writes before returning an opaque InstalledMaterial.
A late/error outcome may leave protected bytes but grants no admission authority.
Exact committed replay checks original operation, every lease field, stored
record digest/timing and actual material bytes; it keeps the old receipt and
local deadline. It cannot extend a lease, overwrite a changed record or recover
an unknown staging prefix as success. The later durable runtime owns recovery.

The installed wrapper can reread bounded verified bytes through its descriptor;
it exposes no arbitrary path and has no Serialize/Deserialize or Debug for secret
material. A historical file is not a live execution capability. Retrieval expiry
and running execution renewal remain different owners. Cleanup is deliberately
absent until an actual stopped/released disposition owns deletion. OS access by
another process with the same unrestricted UID is not a sandbox guarantee;
qualified agent confinement and adapter delivery remain separate integration.

Tests must cover actual accepted/delivered fixtures, byte-preserving replay,
changed operation/material/boot, nonprivate permissions, parent/member symlinks,
hard links, truncated/changed records, quota, expiry during installation and lost
publication acknowledgments. Synchronous fsync work must run on the native
runtime's bounded blocking lane when integrated. No provider call is needed.

This is a trusted local worker-state owner, not tamper-proof storage against a
process that can rewrite the entire private root. The runtime must pin the
returned private-binding digest in its durable preparation journal before sending
Prepare; any later adoption must compare that exact pin. Payload quotas are
logical bounds; filesystem metadata and reserved-space policy belong to the node
capacity owner. The configured root is dedicated to this store.
