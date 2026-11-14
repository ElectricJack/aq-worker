# Worker execution recovery and agent access

**Status:** Current execution contract; foundation recovery/client/source components are qualified, the native consumer is integrated and ran live, including crash recovery, in swarm phase 1 ([`../implementation/0007-P1.md`](../implementation/0007-P1.md)); the agent channel was proven live in [`../implementation/0006-P6.md`](../implementation/0006-P6.md).

**Date:** 2026-10-30

**Reconciled:** 2026-10-31, against the native preparation/grant separation.

**Program tracking aliases:** `Tn` → `WRK-Tn`, preserving the numeric suffix.
The test IDs form one continuous series across the
[worker framework](0001-worker-framework.md) (T1–T20) and this supplement
(T21–T28). These aliases apply to cross-spec tracking; the original tables and
W requirement IDs remain unchanged.

This specification supplies worker implementation requirements for the canonical [shared execution lifecycle](../../../aq-server/docs/architecture/0002-execution-and-recovery.md). The [worker framework](0001-worker-framework.md) remains authoritative for placement eligibility, capacity, cache policy and harness confinement. [AQ's daemon specification](../../../aq-server/docs/specs/0002-execution-and-agent-access.md) owns central authorization and graph delivery. These documents describe complementary mechanisms, not separate task state machines.

## 1. Requirements

| ID | Requirement |
| --- | --- |
| W17 | Follow shared attempt/session identity, launch, expiry, cancellation and result semantics X1–X14. |
| W18 | Supply agents with scoped outbound AQ access while keeping node and graph service credentials private. |
| W19 | Cache only bounded labelled context; never host an authoritative graph replica or acknowledge offline graph edits. |
| W20 | Recover existing agent-operation dispositions without renewing a terminal session's mutation authority. |
| W21 | Publish the worker protocol and conformance fixtures independently of the daemon implementation. |

## 2. Durable execution journal

Extend the framework journal with acceptance ID, reservation deadline, resolved-spec digest, attempt/session/generation, node boot, grant sequence/deadline evidence, launch identity, process evidence, scoped credential reference, pending graph operation IDs where known, and result/directive acknowledgements. Store secrets in separate protected files, not ordinary journal summaries.

Persist before crossing ambiguous boundaries: accepting an offer, starting preparation, launching a process, injecting a message, collecting a checkpoint, uploading a report and acknowledging a directive. Same operation identity with changed bytes is a local conflict. Ordered record transitions and durable file replacement must tolerate process interruption and full disk. Directory synchronization and versioned journal records are part of the storage contract; a single atomic file rename is not a transaction with process spawn or Git.

The runtime has one root owner. Startup validates state format, enters reconciliation and accounts for protected reservations before advertising free capacity. It must discover and stop or safely reattach owned processes even when the prior runtime died before writing a child PID. A launch-specific guardian/process container or equivalent tested ownership mechanism must close that gap. PID existence, tmux pane existence or a missing parent PID alone are insufficient evidence. If ownership cannot be established, report `launch_uncertain`/`stop_uncertain` and protect affected resources.

## 3. Acceptance, grants and stopping

An offer reserves nothing until local admission atomically validates current resources and persists the reservation. Acceptance retries retain their ID and payload. Launch requires the committed daemon grant and a still-live matching reservation. Expired reservations cannot be revived by delayed network responses.

Native acceptance obtains an immutable PreparationPlan, not an execution grant.
The plan and original reservation bound source/artifact/material preparation;
no agent process starts during this phase. Persist the exact preparation operation,
logical launch owner and receipts before reporting readiness. Verify retrieved
artifact bytes and retention descriptors; install private material through the
qualified adapter outside the agent's ordinary workspace grants. Neither material
receipt delivery nor a prepared journal authorizes execution.

A separate grant response must match the current preparation and reservation.
Record its launch intent durably before invoking the host. Publish Started only
from the real verified host observation, retaining the original operation identity
through lost replies. The logical pre-grant guardian identity is not the physical
host's freshly minted guardian nonce; both have explicit binding roles.

Preparation uses its own absolute plan/material deadlines. Execution uses the
shared grant defaults and suspend-aware deadline rules only after grant admission.
Neither response delivery nor exact replay refreshes either deadline. Heartbeat,
renewal and cancellation retain independent capacity. Expiry stops acquisition
and triggers the applicable no-launch cleanup or guarded stop/preserve path.
A late grant response cannot restart the process or restore its credential.

Delivery-lease expiry ends credential retrieval permission. Once the exact private
binding is installed and admitted, a separately versioned running renewal may
continue that same binding without redelivery. Initial GrantedLaunch0.1.6 remains
unchanged. RenewedAuthority must bind the same grant/reservation/owner and original
hard execution deadline, with monotonic authority/grant revisions and a freshly
retimed conservative DeliveryClock. The daemon rechecks current registry/profile,
material fingerprint and machine/agent scope. Rotation/revocation refuses renewal;
the worker rejects stale/out-of-order replies and never reanchors cached TTLs.
Private material is removed after confirmed stop, including stopping triggered
by revocation or the hard deadline. Unknown writers retain protected recovery
state. No renewal authorizes a replacement process or silently refreshes delivery.
This is the required native consumer contract; it is not established by the
existing fixture runtime.


Cancellation acknowledgements identify receipt separately from stop confirmation. Reserve process resources until owned writers are confirmed stopped. Dirty checkpoints, uploads and final reports then proceed under recovery authority and separate storage/transfer bounds. Never delete retained files because the agent credential expired or the task moved on.

A runtime restart creates a new boot ID. Continuation requires proof of the existing process identity and explicit renewed authority for that incarnation. If reattachment is unsupported, stop and preserve; do not launch a replacement under the same attempt ID. New attempts receive new sessions, credentials and writable workspaces, with selected prior source and context supplied explicitly.

## 4. Agent API access

The worker image includes an AQ agent client/tool adapter compatible with the chosen product release. Its implementation is an AQ API consumer and need not link the daemon. The worker owns configuration/provisioning and harness integration; `agent-q` owns client-facing command schemas and domain behavior. The exact client packaging follows the product artifact manifest.

For each authorized session, provide the central HTTPS endpoint, session identity, private token-file binding, API compatibility metadata and allowed local tool configuration. Do not put token values in command arguments, prompts or environment dumps. A harness unable to keep the required runtime credentials outside its configured workspace grants is ineligible for that configuration. Cooperative confinement remains the stated isolation level.

The agent calls AQ directly over outbound HTTPS for graph reads and permitted mutations. Requests do not wait for node polling and do not pass through a worker-hosted graph proxy. The daemon validates live scope and execution authority. The worker does not distribute graph database credentials, graph writer tokens, node credentials or supervisor credentials to agents.

Rotate session credentials through protected node exchanges, retaining only the current permitted binding. On cancellation/expiry/revocation, cease provisioning usable credentials and stop execution; daemon-side checks remain authoritative even if an old file remains readable. Recovery traffic uses node authorization restricted to the node's existing attempts, report/artifact transfers and known operation dispositions. It does not permit arbitrary new graph writes.

## 5. Context cache and disconnected work

The launch context bundle contains task/spec/decision references, record revisions, observation time, truncation and scope identity. Cache size and retention are bounded per node/attempt. Immutable revision content may be reused only within authorized scopes; a cache hit must not widen access. Never retain credentials in a shared context cache.

During a brief disconnect within its grant, an agent can use its labelled context and private workspace. Unknown remote mutation responses remain unknown. Local proposed edits can be preserved as artifacts; they are not committed graph records. On reconnect, new proposals require current authorization and current expected versions. A terminal attempt hands proposals to recovery or a successor rather than acquiring new write authority.

The agent client must return a stable operation ID for uncertain mutations, enabling identical-request retry or status lookup. When the runtime is aware of such IDs, include them in recovery metadata. The daemon's durable intents remain recoverable even if the agent dies before handing its operation ID to the worker; reconcile by authenticated attempt identity through a bounded central listing. No local graph replica or offline semantic merge service is added.

## 6. Messages and results

Persist mailbox receipt before acknowledging it centrally. Deliver session-addressed messages only to the named session. A reused execution slot does not inherit messages. If harness input lacks idempotency, a crash around injection produces delivery uncertainty; it does not justify an automatic second injection. Task-addressed rerouting is a central policy decision.

Reports include immutable identity/digest, assignment/spec binding, process outcome, stop certainty, checkpoint/source and central receipt references, log coverage and any unresolved remote operation references. Submit success claims separately from observed exit status. Source retention, report disposition and graph completion are independently visible.

Stopped execution releases its slot only under the framework's writer checks. Pending bytes remain protected and bounded. A refused late result may be retained for review; release requires the appropriate retention disposition rather than a successful-task flag.

## 7. Protocol publication and compatibility

The [profiled tmux/session supplement](0003-profiled-tmux-sessions.md) adds frozen launch manifests, required adapter capabilities, terminal identities, controller fencing and bounded outbound terminal exchanges. Publish these through CON-02 before changing consumers. Agent API access profiles remain distinct from agent, intelligence and execution profiles. Terminal commands do not renew execution or alter agent grants.

[`protocol/README.md`](../../protocol/README.md) defines the owned contract artifacts. Publish a separately versioned execution protocol with node/boot binding, negotiated capabilities, stable IDs/digests, typed errors, bounded pagination and explicit acknowledgement meanings. Foundation enrollment/heartbeat/probe version 1 retains its existing interpretation.

The protocol contains no private daemon/graph tables or scheduler implementation. A small optional Rust wire-types crate may be consumed by both repositories; it must remain implementation-independent. Schemas, fixtures and Rust types derive from one declared contract source with CI drift checks. A schema version cannot substitute for behavioral compatibility tests.

Version negotiation happens before acceptance. Unknown required capabilities decline an offer. Unsupported execution nodes can remain connectivity-visible where foundation compatibility allows it. Compatibility tests cover the AQ release's advertised supported window, including rolling central upgrades and delayed worker updates.

## 8. Tests and delivery sequence

Use fake clocks, a fake coordinator and deterministic harness for core tests. Fail at every journal/action boundary, delay/reorder responses and restart with surviving fake/real children. Assert phase, authority, protected resource and acknowledgement invariants rather than merely serialization output.

| ID | Required demonstration |
| --- | --- |
| T21 | X1–X3 and X9: replay/expiry/spawn crashes never cause blind duplicate launch or unsafe resource reuse. |
| T22 | X5 and X11: suspend, expiry and credential revocation stop acquisition; graph writes fail centrally while retained work survives. |
| T23 | Context snapshots remain scoped, bounded and labelled; disconnected proposed writes are never presented as applied. |
| T24 | Recover commit-before-response-loss through known operation/attempt identity without restoring terminal-agent write permission. |
| T25 | X6–X7 and X12: cancellation and late results preserve evidence but do not settle newer attempts; central retention outlives the node. |
| T26 | X13–X14: ambiguous message injection stays explicit and bulk load cannot starve control requests. |
| T27 | Protocol fixtures validate both current and supported older peers, and connectivity-only foundation nodes remain valid. |
| T28 | Run agent client configuration against a fake AQ API without aq-graph, the daemon or a sibling source checkout. |

First publish schemas/fixtures and implement the durable state machine with a fake agent. Then add one real harness and protected agent-client provisioning, aq-git handoff, graph-operation recovery and compatibility testing. Extract the existing foundation connector through an explicit migration/release step. Do not maintain two production enrollment implementations or claim this design itself performs the extraction.

## Current implementation boundary

The qualified worker includes foundation execution/recovery, scoped AQ client
provisioning, local aq-git workspace and retained-source handoff, and a guarded
tmux host. The native acceptance/preparation/grant journal and first real harness
consumer still require assembled qualification. Terminal bridge stage1 and the
experimental observation store have separate component evidence and are not yet
adopted by the production worker. Their presence in the workspace does not
authorize advertising those capabilities. See the
[completion map](../../../aq-server/docs/plans/0002-execution-sessions-policy-completion.md).
