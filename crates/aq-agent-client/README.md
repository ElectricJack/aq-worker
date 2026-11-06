# AQ agent client

`aq-agent` is a finite, session-scoped API 1 CLI and Rust client. It provides live
or explicitly cached context, six graph reads, graph commands, existing
operation status, mailbox messages and task comments. It consumes the
separately packaged public `aq-agent-api` contract, without linking the daemon
or graph database. Terminal control, suspension and provider harness launching
are not exposed by this CLI.

An independently installed local result channel supports one explicit completion
claim. It uses no Graph credential or HTTP client configuration:

```sh
"$AQ_AGENT_RESULT_COMMAND" result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"
"$AQ_AGENT_RESULT_COMMAND" result status
```

The request is only `{"schema_version":1,"conclusion":"Changes; exact checks and outcomes; limitations"}`.
Its conclusion is a nonblank UTF-8 line of at most 512 bytes; oversized or unknown
fields are rejected. The original attempt-bound IDs and frozen expected outcome
come from worker-published control data. The client atomically publishes at most
one immutable local claim. Repeating the same claim reads status; a different
claim conflicts without replacing the original. After a new publication, submit
waits at most two seconds, capped by the remaining original execution deadline,
for a matching worker acknowledgment. Timeout returns pending without retrying
or changing IDs. Exit code 10 alone means pending; inspect the phase to distinguish
local publication from worker acceptance. Stop editing after submission.

The result root lives outside captured source. The sandbox maps control files
read-only and only `submissions/` writable. A retained directory/binding identity,
existing worker lock, boot identity and both original BOOTTIME execution/hard
deadlines guard new publication. These are refusal checks, never live execution
authority. The worker independently requires its actual retained execution owner,
obtains physical stop and central source retention, then records the report.
After execution expiry, a matching completion-owner lock on the same boot can
still support accepted/finalizing status; it never admits a new claim. After
owner loss, status is historical/recovery data and new claims are refused.
An exact `recorded` acknowledgment means report recording only; review, task
completion and resource release remain separate. No automatic replacement or
renewal exists after an uncertain write. The unchanged original request and IDs
must remain available for status/recovery.

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

Task comments (aq-server `protocol/agent-comments/v1`, pinned in
`vendor/agent-comments-v1` with `vendor/agent-comments-v1-source.json`) let the
agent record findings on its own task, which the owner sees in the task view:

```sh
aq-agent --state-dir DIR comment add "Finding: the adapter drops \`executions\`."
aq-agent --state-dir DIR comment add --file notes.md      # or the body on stdin
aq-agent --state-dir DIR comment list [--limit 20] [--cursor tc1.N] [--json]
```

The body is Markdown, 1..16384 bytes, not blank, with no control characters
other than tab, LF and CR; it is checked before anything is sent. A trailing
newline from `--file` or stdin is dropped. The credential binding chooses the
task, session and attempt; the request names none of them. Each add sends once
with a new operation UUID (the comment ID). After an uncertain outcome the
stderr JSON carries that `operation_id` with `"retryable":true`: rerun with
`--operation-id` and the same body, and the server returns the original comment
instead of a duplicate. The same ID with a different body is `id_conflict`
(exit 11). `list` shows the task's thread newest first, including comments from
earlier attempts and the owner; pass the printed `--cursor` for older ones.
Without `--json` both print text; with it, the validated response JSON. Agents
cannot edit or delete comments.

Availability: an agent can run `comment` only where its runner installs the
per-attempt state directory (`client.json`, `context.json`, `credential.json`)
and the AQ origin is reachable. The sandbox runtime image ships
`/opt/aq/bin/aq-agent`, and the B7 runner path (`AgentAccess` behind the B6b
loopback proxy) installs that directory. The MS1 docker-local Claude driver
(`aq-worker` `sandbox_driver`) does not yet: it issues no agent credential,
mounts no state directory and gives the agent no route to aq-server, so its
runner note does not mention comments.

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

Initial local result source qualification: seven completion file/CLI tests passed
(0.06 seconds), five library tests passed (2.12 seconds, including the existing
three journal tests), and all-target strict Clippy passed (3.73 seconds). The
fixtures exercise a real client process but synthetic presence/acknowledgments;
they create no worker grant, actual stop, source receipt or B1 report. Harness
installation and worker producer integration require their own qualification.
The bounded acknowledgment wait and post-execution completion-owner status
passed nine focused completion tests (2.06 seconds), including delayed matching
acknowledgment, the shorter original execution ceiling, finalization after lease
expiry and dead-owner refusal. All five pure contract tests passed, both crates'
all-target strict Clippy passed (1.74 seconds), and the default binary built
(3.36 seconds). The exact rebuilt binary also passed the CLI subprocess vector.
The final unclaimed-expiry status correction passed its exact expiry vector
(0.03 seconds), all-target strict Clippy (1.53 seconds), default binary rebuild
(1.81 seconds), and the exact rebuilt-binary CLI vector. The unchanged nine-test
and pure-five gates above were not repeated for that one-condition correction.
The post-publication control-fault regression then passed (0.08 seconds): failed
acknowledgment inspection returns the original IDs/digest with effect possible,
while preserving the actual published bytes. Strict Clippy (1.42 seconds), the
default rebuild (1.91 seconds) and exact rebuilt-binary CLI vector also passed.
