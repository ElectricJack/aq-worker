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
- `POST /v1/worker/slots` (minting one node per slot) is for old workers only.
