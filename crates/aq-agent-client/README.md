# AQ agent client

`aq-agent` is a finite, session-scoped API 1 CLI and Rust client. It provides live
or explicitly cached context, six graph reads, graph commands and existing
operation status. It consumes the separately packaged public `aq-agent-api`
contract, without linking the daemon or graph database. Messaging, terminal
control, suspension and provider harness launching are not exposed by this CLI.

A trusted worker calls `provision::Allocator` with the exact `StartupContext` and
`node::NodeClient`. Allocation verifies the published profiled offer, manifest,
API binding and exact required context bytes, reserves bounded local storage,
then records the credential-issuance intent before HTTP. The daemon remains the
owner of current authority. Successful issuance publishes the managed client
configuration last. A lost response stays unknown; reopening never implicitly
issues another credential. Explicit recovery or rotation uses a new intent and daemon CAS; unknown issuance may have failed before or after commit.

The agent receives only its private session directory and this executable:

```sh
aq-agent --state-dir /absolute/private/session context
aq-agent --state-dir /absolute/private/session context --cached
aq-agent --state-dir /absolute/private/session graph get NODE_UUID
aq-agent --state-dir /absolute/private/session graph command --request-file request.json
aq-agent --state-dir /absolute/private/session operation get OPERATION_UUID
```

Command JSON follows the packaged `contracts/schema.json` `CommandRequest`.
The caller supplies an operation UUID. Canonical request bytes, cumulative retry
count and suspend-aware deadline are durable before sending. Each invocation
sends at most once. After uncertainty, query the existing UUID; repeat only the
original request if authority and its retained retry budget still permit it.
A not-found response is not proof that no effect occurred. Terminal receipts are
retained; neither full storage nor an exhausted retry budget silently evicts or
replaces an operation. A host boot change or backwards clock refuses retries.

Stdout contains public JSON results. Exit codes are 0 for reads/confirmed effects,
10 pending, 11 conflict, 12 rejected, 13 unknown, 14 authentication refusal and
15 invalid input/private state. Safe failures go to stderr with no raw transport,
credential or filesystem error. Credentials are never command-line arguments.
The optional `--token-file` is only an assertion of the managed credential path.

The worker node credential and graph-service token are never installed into the
session directory. If the deployment needs edge authentication, the worker must
provide a separate agent egress identity; node identity reuse is refused. All
requests are outbound HTTPS (plain HTTP only for loopback fixtures), with fixed
routes, no redirects, bounded responses and finite deadlines.

`NodeClient::recover` is library-only. It can query known or centrally listed
operations for its own attempt and request an exact retained retry. It cannot
supply a new graph payload or renew an agent's write authority.

Local storage uses owner-only directories/files, no-follow descriptor-relative
file access, exclusive ownership and atomic fsync/rename writes. A node allocator
reserves at least 64 MiB per session plus 2 MiB of node metadata, with count and
physical free-space checks. Each session retains at most 128 graph intents in a
16 MiB journal, 8 MiB of context and 16 credential issuance/rotation intents.
Required context is never silently truncated. Optional absent content is labeled;
cached context never grants authority. These application bounds are not a disk
quota against another process using the same UID. Such agents share the normal
worker trust boundary; stronger hostile-agent isolation needs separate identities.
There is no automatic reclaim until the owner proves recovery and retention safe.

The library returns exact startup text segments in manifest order; a harness
adapter must preserve those bytes and mount the managed private directory.
Installing it into an actual provider harness and worker image is a separate
WRK08/product release qualification. This component alone grants no process launch.
The public schema includes routes beyond this CLI's capability list; consumers
must not infer that this binary implements every route in the public API.

Build/test without daemon, graph, Git service or provider credentials:

```sh
cargo test -p aq-agent-client --locked
python3 scripts/package-agent-client.py --output /tmp/aq-agent-client.tar.gz
```

The distribution contains the host-target binary, public contracts, instructions
and a byte-hashed manifest with source revision, dirty state and compiler details.
It does not contain credentials. It is a local artifact; the script does not
publish, install or modify product release selections.
