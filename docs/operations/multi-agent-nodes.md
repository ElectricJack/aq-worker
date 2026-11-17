# A worker node that runs several agents (wire contract)

The full specification is aq-server `docs/specs/0015-multi-agent-nodes.md`. This note lists
what the worker sends and receives. The owner's requirement: "worker nodes can run more than
one agent at a time with more than one work item at a time."

A node has one identity, one credential and N **slots**. N is the server's Setting for the
node. Queued attempts and hand-started (manual) sessions share it.

| Where | Field | Type | Meaning | Default |
| --- | --- | --- | --- | --- |
| `POST /v1/worker/control` request | `schema_version` | `u32` | `2` (`aq_protocol::NODE_SLOTS_SCHEMA_VERSION`) asks for the slot count. `1` is the old request. | A server that answers 400 `bad_request` to `2` is an old server: ask again with `1` and run one slot. |
| `POST /v1/worker/control` reply | `slots` | `u32`, optional | The node's slot count, `0..=1024`. `0` admits nothing new. Sent only in a `schema_version: 2` reply. | Absent = 1 (`ControlResponse::node_slots()`). |
| `POST /v1/worker/profiled/register` | header `aq-node-slots` | decimal `1..=1024` | The slot count the node runs under this boot. Its presence says the worker is slot-aware: one registration and one boot id per service lifetime. | Absent = a worker that registers per attempt and runs one attempt per node. |
| `POST /v1/worker/profiled/poll` | header `aq-node-slot` | decimal `1..=1024` | The 1-based slot this poll is for. At most one offer comes back, and it is for this slot; `after_offer` pages this slot's undelivered offers only. | Absent = slot 1. |

Header names, the bound and the parsers are in `aq_admission::node_slots` (aq-admission 0.1.13).
The transport documents (`Register`, `Poll`, `PollPage`) are unchanged: aq-server pins their
source and schema bytes, so the slot travels beside them, like `aq-delivery-nonce`.

Rules for the worker:

- Send the two headers only to a server that answered a `schema_version: 2` control request.
- Every slot loop uses the node's one boot identity and its inventory sequence, with its own
  operation ids. A slot above the current count gets empty pages; its running agent finishes.
- Do not register again when the count changes. Read the count from each control reply.
- All other native requests (accept, prepare, started, renew, material, artifact, stopped,
  results, launch closure, lost-attempt release, handoff notice) are unchanged. They name
  their attempt, and the server finds the slot from the attempt.
- After a service restart the node registers its new boot at once. An attempt of an earlier
  boot keeps its slot until it is resolved by a stopped report, a launch closure, a
  lost-attempt release, or the server's never-started lapse. Those requests carry the
  attempt's own (earlier) boot identity and are accepted. An attempt of an earlier boot can
  no longer start, renew, or fetch material or artifacts.
- `POST /v1/worker/manual-agents`: `max_slots` stays in the body (`1..=8`) for old servers. A
  slot-aware server ignores it and admits manual sessions against the node's slot count.
- `POST /v1/worker/slots` mints additional enrolled **nodes** for a machine. Increasing a node's agent count never calls it.

## Machine, nodes and agent slots

A machine can run several enrolled nodes. `service.toml` calls this count `nodes` (1..=8);
old files with `slots` still load with exactly the same meaning. `aq-worker service enroll
--nodes N` mints node credentials, and accepts the historical `--slots` alias. An update never
folds or renames the existing nodes. Each node polls control using its own credential and gets
its own agent count (0..=1024). On an old server each node runs one queued agent and retains the
legacy manual-session behavior.

Each node has one boot, one successful registration, one inventory stream and one connector
heartbeat for this service invocation. Register retries use the same boot and operation id.
Changing the server count does not register another boot. A slot failure or refusal does not
reset the node session. The registration count is pinned on the first registration attempt;
the live server control count governs later admissions.

The service isolates each `(node, agent slot)`:

| Resource | Agent slot 1, including previous builds | Agent slots 2 and above |
| --- | --- | --- |
| Enrollment | `config/slots/K/worker.toml` | Same node K enrollment |
| State, journal, material, Git root | `state/slots/K/current/` | `state/nodes/K/slots/N/current/` |
| Held reason, archives | `state/slots/K/{held,archive}` | `state/nodes/K/slots/N/{held,archive}` |
| Git socket and session host | `runtime/K/{git.sock,host}` | `runtime/K-N/{git.sock,host}` |

A previous build's `current/host` stays in use while that attempt recovers, since its journal
already names those paths. New agent slots have no enrollment files. The service checks socket
path lengths before it admits a higher count. Sandbox and agent-channel scopes include the
attempt identity; cleanup lists by node and then filters by attempt, including when sibling
attempts share a boot.

Growth starts new slot loops. Shrinkage pauses new admission on the highest slots, lets their
running attempts finish, recovers any outstanding journal and retires the idle loops. Retained
state and archives remain available. On restart, every discovered slot recovers independently,
even above the new count. One held slot does not prevent the node from registering or its
siblings from running. Superseded-boot reports keep their original identity.

Queued polling and manual sandbox creation reserve from one local count, in addition to the
server's authoritative count. Pending manual launches pause idle queued polling long enough
to acquire a free permit; running queued attempts finish. A manual session releases its permit
only after a confirmed stop. `[manual] max_slots` still parses for old-server compatibility;
it is ignored for slot-aware nodes and omitted from newly written configuration.

## Explicitly reducing machine node count

An eight-node machine remains eight nodes after a binary-only update. To run two, the operator
must lower `nodes` to 2 (or the old `slots` key to 2) and restart the service. Nodes 3..8 are
discovered from their existing enrollment directories. They recover or release their own
outstanding attempts using their own credentials, take no new work, do not register a new
boot, and stop once recovery completes. A held recovery remains visible and retained. The
server/operator separately marks these nodes removed. Their credentials and history are kept.
The current binary-only remote receiver preserves node count; it does not carry a node-count
change. The local installer/enrollment `--reconfigure --nodes N` is another explicit change.

`service status` lists nodes and their retained slots. For a sandbox in node K, agent slot N,
use `service release-sandbox --node K --slot N ...`; retired nodes remain addressable. Without
`--node`, the historical `--slot K` command still addresses node K's first agent slot.

## Server behavior required

The server must implement the v2 contract, per-slot offer redelivery and attempt correlation,
shared queued/manual admission, and terminal reports for superseded boots. The worker
serializes inventory publication with polls, cancellation polls, and the final inventory
refresh through phase-execution admission, so sibling publications cannot invalidate those
requests. It does not interpret an arbitrary conflict as permission to register a new boot.
An old server's v2 `400 bad_request` is cached as a v1 fallback for the link's lifetime;
revocations, edge errors and transient failures never trigger that fallback.
