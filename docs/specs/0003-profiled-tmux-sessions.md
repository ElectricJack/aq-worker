# Profiled tmux sessions and outbound terminal access

**Status:** Implementation design; current synthetic-worker qualification does
not establish this functionality.

**Date:** 2026-10-30

The canonical policy, definitions, limits and P/H/R requirements live in
[Agent profiles, intelligence, tmux hosting and remote terminals](../../../agent-q/docs/architecture/0003-agent-profiles-harnesses-and-terminals.md).
This specification defines worker mechanisms and independent tests. The existing
[execution contract](0002-execution-and-agent-access.md) still owns grants,
journal reconciliation and source retention.

The [observation and suspension supplement](0004-session-observation-and-suspension.md)
adds required transcript readers, accounting capabilities, safe nudge arbitration
and physical hibernation/continuation startup. Native usage support is qualified
per the extended product matrix; it is not established by terminal hosting tests.

## 1. Boundaries and packages

The daemon resolves profiles and selects concrete model/provider/harness settings.
Workers validate the sealed launch manifest, installed adapter/CLI versions,
credential-profile references, environment, local mount aliases and available
capacity. Refuse unsupported required settings before acceptance; never select
a different model, relax confinement or install arbitrary software to fit an
assignment. Mutable aliases resolve locally only to operator-declared roots.

CON-02 extends the public protocol with independently versioned launch and
terminal envelopes/fixtures. Preserve older foundation behavior and decline
unsupported new capabilities. Do not repurpose the API-01 graph access profile
as the agent/execution/intelligence profile.

WRK-10 introduces a small public Rust session-host crate in this repository,
with typed lifecycle/configuration primitives and a deterministic test harness.
Harness adapters use that crate; worker authority/journal integration stays in
the runtime. The daemon's central supervisor host may consume the pinned crate
and adapters without importing node identity, worker scheduling or private state.
Central sessions receive their own authority adapter under AQ-17. No additional
runtime service or worker database is required.

## 2. Launch and process ownership

1. Validate the manifest and protected context/credential material while the
   reservation/grant is live. Journal the immutable manifest digest and local
   environment/adapter version observations.
2. Prepare the aq-git worktree under existing source rules. Materialize startup
   instructions once from the authorized digest-bound bundle. Keep node and
   provider credentials out of ordinary prompts, argv, terminal streams and
   shared context caches.
3. Journal the attempt-specific tmux socket/server/session identity and launch
   intent before effects. Start a fresh server inside the guardian boundary;
   never attach an agent to a general user/shared server.
4. Launch the selected harness in its pane only after the guardian's durable
   authorization/start handshake. Construct arguments without shell interpolation
   of untrusted profile or task text.
5. Observe readiness and process outcome through the adapter. Pane identity,
   terminal epoch and process/guardian evidence bind to the exact attempt/session.
   Tmux startup or visible text does not establish successful agent initialization.
6. Stop on cancellation/expiry using the independent guardian. Include all
   tmux/agent writer descendants, reap them and persist proof before checkpoint,
   handoff or resource release. Acknowledged tmux commands are not stop proof.

The hosting qualification must establish actual server ancestry/daemonization,
including crashes before/after socket creation and pane launch. If the proposed
tmux mode cannot preserve the guardian proof, WRK-10 is incomplete; do not weaken
the ownership invariant. Account for per-attempt tmux and terminal-buffer costs.

Browser detach does not stop the process. Worker restart retains the existing
stop-and-preserve policy unless a separate live-reattachment profile is qualified.
Guardian loss stays uncertain. Terminal reconnect attaches only to an already
verified session and cannot recreate a server, pane or agent. Harness exit never
spawns a fallback shell or gives a viewer control of a reused pane; retained
output after stop is read-only and separately bounded.

## 3. Harness adapters

The adapter interface validates exact configuration, emits native config/argv,
observes readiness/outcome, describes permission semantics, supplies initial AQ
context/tools, handles supported message delivery, and cooperates with stop.
The host owns tmux, transport and process ownership uniformly.

Codex CLI is WRK-08's reference adapter. Claude Code (WRK-11), Gemini CLI
(WRK-12) and OpenCode (WRK-13) independently qualify the same minimum contract.
Each records exact CLI/adapter versions and a compatibility matrix for models,
permission settings, folder grants, tools, messages, transcripts and usage.
Do not infer support from another provider's flags or generic terminal access.

Every target must support initial scoped AQ context/tools, tested default
worktree confinement, tmux watch/control and safe stop. A required unsupported
grant declines the attempt. Native message injection, conversation resume and
structured usage/transcripts are advertised separately; absence cannot silently
fall back to unrestricted execution or input at an unsafe prompt boundary.
WRK-09 consumes these capabilities and may use the authorized AQ inbox when
declared. Harness state observations do not themselves complete graph tasks.

## 4. Local terminal bridge and journal

WRK-14 provides a bridge for the verified attempt-owned socket/pane. Only typed
watch, literal input/paste and resize operations are exposed; no arbitrary tmux
command, socket path or host shell endpoint is accepted from the network.
Retain local controller generation/expiry and terminal epoch in the journal.
Validate boot, assignment generation, session, controller and command deadline
again immediately before input effects.

Journal received command identity/digest, then `injecting` before the tmux call.
Persist `applied` only after a confirmed local handoff. A crash in between yields
`delivery_unknown`, blocks later interactive input pending explicit resolution,
and never blindly replays the bytes. Literal input acknowledgement does not mean
the model read or acted on it. Resize is a separate idempotent ordered operation.
Expired queued input is dropped with an explicit disposition. Stop directives
have priority and are not implemented by simulating a Ctrl-C key.

Capture bounded output with epoch/sequence/digest and a snapshot-watermark
barrier. Test full-screen/alternate-screen behavior rather than assuming plain
capture-pane text reproduces terminal state. Overflow emits gap markers and a
new snapshot/epoch; local output draining cannot block the agent or guardian.
Interactive buffers can discard acknowledged/expired output within policy;
source/results and explicitly retained transcript artifacts use their own
durability rules. The shared design supplies initial byte/concurrency limits.

## 5. Outbound transport

All connections originate here. A separate bounded long-poll request retrieves
terminal subscriptions, controller changes and input. Output/receipts upload in
bounded batches with cursors. Identical retries deduplicate; changed payloads
conflict. Worker code contains no inbound listener or general reverse proxy.

Use independent request/connection budgets for execution control and terminal
traffic. No terminal long poll is held when there are no subscriptions or pending
control work. One per-node channel multiplexes bounded active sessions; do not
create a control long poll per agent. On disconnect, retain only bounded output,
expire input/control permits and keep enforcing the execution grant locally.

## 6. Required tests and product evidence

| Suite | Required evidence |
| --- | --- |
| Manifest/configuration | Unknown schema/capability, changed digest, denied required tool, stale adapter version and unavailable mount/credential profile refuse without launch. |
| Common host | Actual tmux plus synthetic interactive child; identity/socket collisions, all launch crash boundaries, adopted/double-forked writers, guardian death, expiry, cancellation and read-only post-exit inspection. |
| Each named harness | Mock config/transcript fixtures without credentials; separate explicit real-CLI/model launch, confined edits, terminal interaction, native capability refusals, stop and retained source. |
| Input | Same-ID retry before effect, lost ack after effect, crash during injection, controller takeover/revocation/expiry, stale boot/session/pane reuse, no implicit replay after reconnect. |
| Output | Snapshot/live race, sequence conflict, Unicode/alternate screen/resize, buffer overflow, reconnect and stale read-only display. |
| Resource isolation | Output flood and slow coordinator/viewer while renewals/cancellation meet the existing control guarantees; enforce local aggregate bounds and prevent PTY blockage. |

Normal component tests use fake coordinators/providers and do not require
agent-q source, PostgreSQL or live credentials. Actual tmux tests are separately
labelled local process tests. Real provider/CLI qualifications are explicit
versioned suites, not implicitly run on every unit-test invocation. INT-02 owns
the final central/worker/browser integration across all four named adapters.

## Revision-pinned implementation evidence

The [bounded v1 reference inventory](../../../agent-q/docs/research/2026-10-30-v1-implementation-reference-map.md) records implementation symbols, selected test obligations and per-file hashes at revision `9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`. It distinguishes preserved behavior from remote-authority and retention adaptations. This evidence informs the owning v2 tasks; replacement coverage and product qualification remain pending.
