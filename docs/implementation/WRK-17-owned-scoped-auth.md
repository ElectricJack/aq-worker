# Owned native Responses transport and launch-bound child auth

Status: bounded source candidate with focused file/HTTP tests. Production
admission remains refused. Named-image composition, process qualification and
credential-source isolation are separate requirements; this candidate does not
make the original provider material inaccessible to a same-UID process.

## Original owner and fresh derived input

The pipeline resolves the actual original InstalledMaterial through retained
execution inputs and its independently pinned receipt/binding. An additional
retained descriptor references that same directory and immutable metadata; it
creates no second installation, retrieval lifetime, resource claim or journal.
Every read now rechecks the canonical original record and exact material bytes,
private modes and nofollow regular-file checks. Manifest/credential/provider
association is validated before creating the scoped transport.

`ScopedResponsesTransport::start_retained` verifies the exact retained manifest
against the full LaunchBinding and selected model/reasoning/cap, then generates a
fresh 77-byte local capability with 244 random bits from two UUIDv4 values. It
keeps a distinct upstream key in the transport owner. The scoped local listener
and HTTPS route obey the bounded transport policy; test-support alone permits a
numeric-loopback upstream. Before each upstream request, a bounded read-only job
rechecks the original installed material. A changed record or key refuses the
request before forwarding. Cancelling a pending read cannot authorize a later
request; local read-only storage jobs may finish after their async caller ends.

The child auth constructor takes original material pins and one closed
ScopedCodexAuthBinding: full LaunchBinding, launch ID, numeric loopback origin,
original deadline and capability digest. Only the local capability is encoded in
the child's auth.json. The raw provider key is not written to that derived file,
argv, command environment, host intent or worker journal. The helper still reads
original bytes transiently to verify their digest before execution.

The existing launch intent configuration digest binds original source pins,
new private directory/auth identities, derived auth digest, exact local endpoint
and launch/deadline metadata. Empty owner-only files are reserved first. The
existing authority must durably record that intent before auth bytes are
published. Publication and the helper's final exec boundary recheck original
and derived bytes. The helper verifies the full manifest/binding, original
permit expiry and exactly one matching local provider setting.

This adds no secret journal or external proxy service. Request bodies are
bounded and credential values are not logged; opaque owners are not Debug or
Serialize. Retained key/capability buffers are zeroized, with no claim about
zeroization of every reqwest-internal request/header allocation.

## Stop, failure and recovery

The process-local launch entry retains its transport before private publication
or process launch. The logical safety fence synchronously cancels that transport,
including when the returned physical-stop future is never polled. A bootstrap
that installs an owner after an earlier fence cancels it immediately. Expiry or
cancellation closes active upstream/downstream forwarding and its listener;
no request is replayed by this owner.

Physical stop continues through the original exact host recovery path. Only a
matching opaque ECHILD proof permits derived auth cleanup. After physical stop,
the async owner joins transport closure; cancellation of this join still aborts
its retained task handle. A failure to prove physical stop preserves intent,
retained input ownership and reconciliation obligations. Reopening a journal
never recreates a local capability, transport, key or execution Grant.

Local IO closure does not prove rollback or cessation of work already accepted
by a remote provider. Per-response output caps are independent of aggregate
spending or task-lineage limits; this adapter still refuses a nonempty spending
budget.

## Required boundaries that this source does not supply

A same-UID harness that can see the original material path or worker process
`/proc/<pid-or-tid>/fd` namespace can bypass a local capability and read the
original material. Mode0700/0600, a fresh child auth file and a read-only executable
image do not separate that process from the worker's credentials. The SourcePin
intentionally identifies the original worker descriptor for the helper's
pre-exec recheck. Production needs an independently qualified credential boundary
after that check: source-path and parent-process visibility confinement or a
suitable separate identity/process namespace policy. All thread/process aliases
and other worker credentials must be covered. No boolean configuration claim can
substitute for that qualification.

As defence in depth, the scoped final command omits AQ_SESSION_DIRECTORY and
other unrequested worker environment values; legacy fixture behavior remains
separate. CODEX_HOME and inferable paths can still reveal nearby layout. Removing
a metadata hint is not the credential isolation boundary.

The named read-only image profile supplies image immutability evidence only.
It does not imply presence of scoped auth, an enforced output cap, trusted tools,
context counting, credential isolation or provider eligibility. The worker's
joined NativeCodexHostProfile explicitly constructs scoped auth and an owned
transport; its profile digest names that joined behavior. Other image-only or
legacy direct-auth users cannot advertise those joined capabilities by inference.

Further gates remain: actual effective CLI/tool behavior under the exact pinned
named image/environment, independent harness egress confinement and access to
only its local transport, whole implicit-context/tool token accounting, provider
semantics and credentials, and fresh component artifact packaging/pins. No
production factory is enabled by these source changes.

## Focused verification

The initial candidate passed five host private/scoped filesystem cases, two real
original-file/loopback retained-transport cases and seven transport cases. Strict
host/worker all-target/all-feature Clippy passed. They cover exact publication and
secret-free metadata, original source mutation before exec/forwarding, fresh
capability separation, manifest substitution and route/deadline checks. These
are not actual Codex/provider or credential isolation qualifications.

Additional safety-fence/env and static-peer physical ECHILD cases accompany the
joined source; their final composed results are recorded by the integration lane.
No paid provider call is required for these tests.
