# Session observation and suspension mechanisms

**Status:** Implementation design; not qualified by the existing synthetic runtime.

**Date:** 2026-10-30

The canonical [session operations, waits and notifications design](../../../agent-q/docs/architecture/0004-session-operations-waits-and-notifications.md)
owns S/N/C requirements, policy, budgets and domain semantics. This supplement
extends [profiled tmux hosting](0003-profiled-tmux-sessions.md) and the existing
[execution/retention contract](0002-execution-and-agent-access.md).

## 1. Contracts and independent modules

CON-03 publishes versioned worker observation, parser provenance, suspension,
checkpoint, retained-continuation and bootstrap-ack fixtures. Human notification
and wait-registration APIs remain AQ-owned. Preserve older protocol behavior;
decline unsupported required capabilities rather than silently treating a wait as
task success. Neither transcript events nor agent claims can assert authoritative
graph outcomes or create permission to restart a process.

WRK-15 introduces small Rust transcript-reader/normalization modules with fixture
input and no daemon, database or provider dependency. They are reusable by the
central host through public packages. Runtime journal/transport integration is
separate. WRK-16 owns physical suspension and retained continuation preparation,
not dependency evaluation or wake scheduling. No external-notification credentials
or Discord transport belong on a worker.

## 2. Observation and safe interaction

Identify the transcript/event source through the launched harness session. Validate
the path/handle against authorized local roots and the expected session identity;
reject symlink/path changes that redirect collection into unrelated data. Parsers
consume bounded records incrementally and emit source identity/generation,
record/call identity, cursor or byte range, digest, version and observation time.
Tie native resume lineage to prior call identities so historical usage is not
counted again. Counter reset/rotation cannot produce negative or fabricated usage.

Persist events to a bounded durable outbox together with the cursor that consumed
them. Send replayable batches over outbound worker exchanges and retain them until
the coordinator's receipt or explicit bounded-loss policy applies. Record gaps
and accounting incompleteness when evidence cannot be recovered. Never advance a
cursor and claim complete accounting for facts not durably recorded. A parser
version upgrade has an explicit replay/correction protocol.

Coalesce ordinary state observations; do not upload every terminal repaint or
update task graph rows for heartbeat/token changes. Parser backlog, malformed
records, source disappearance and node offline are different observations. Usage
metadata excludes raw prompt/tool content unless separately authorized. Raw
transcripts use scoped artifact retention, not the disposable terminal ring.
Minimal accounting facts can outlive expired transcript content under policy.

Peek and attach reuse the bounded snapshot/terminal bridge. WRK-09's nudge path
uses the durable message identity and the adapter's qualified readiness/composer
guard. The same local input arbiter excludes nudges while a human controller owns
the pane. Inbox fallback/deferred/unsupported is visible; never paste then press
Enter at an unrecognized prompt. Unknown injection is not automatically retried.
Stopped or suspending attempts reject new ordinary input, including delayed messages.

The worker reports observations and executes admitted typed directives. AQ owns
the stall ladder; local telemetry cannot autonomously replace an agent or relax
its expiry. Stop/renewal/source-recovery capacity remains independent of transcript
spooling and terminal streaming.

## 3. Physical suspension

1. Receive a durable suspension directive bound to wait, continuation generation,
   session/attempt, node/boot and assignment generation. Journal its payload digest
   and receipt before effects; repeats recover the same transition.
2. Request/retain the bounded checkpoint summary through the scoped AQ API. Its
   receipt is not stop proof. Missing summary follows an explicit recovery policy;
   the runtime must not invent completed work or discard dirty source.
3. Close new input and ordinary agent operations as directed by AQ, then stop
   every owned writer through the existing guardian. A terminal prompt, process
   exit notification without ownership proof, or agent end-turn is insufficient.
4. After confirmed stop, take the supported aq-git snapshot of final source state,
   including supported dirty/untracked work. Preserve references to pending domain
   effects for central reconciliation; the worker cannot declare them cancelled.
5. Reuse resumable artifact/source upload and exact retention receipts. Report stop
   certainty, local retained bytes, central receipts and suspension disposition
   separately. A dependency outcome arriving mid-stop cannot restart the old process.
6. Release compute after stopped-writer proof according to the existing protocol.
   Keep required disk protected until explicit release; safe migratable hibernation
   requires central retention and effect reconciliation. Crash/restart resumes the
   same local operation, never another hidden agent launch.

Report a distinct suspended disposition. A cooperative exit during parking must
not be emitted as task success/failure merely because the harness exited; AQ owns
task settlement and distinguishes planned suspension from a failed recovery.

If the node is lost before retention or stop proof, AQ holds conflicting admission.
If central retention and proof are complete, a sleeping continuation no longer
requires this node or its hot worktree. Unsupported snapshot content, missing
proof or storage exhaustion is an explicit hold, not permission to silently drop
files. Expired agent credentials do not block already-admitted source retention
through the node recovery path.

## 4. Continuation startup

A wake is a normal new grant/attempt with a continuation attachment: predecessor,
generation, retained source and summary digests, verified wait outcome references,
fresh resolved profile/context, held task mail and remaining budgets. Validate
compatibility and fetch only authorized retained inputs. Local cache availability
is an optimization, never a requirement for successful wake on another node.

Start a fresh tmux session through the common host. Supply bootstrap instructions
to read and acknowledge the exact continuation generation using fresh scoped
credentials. AQ enforces the bootstrap mutation restriction; the worker does not
fabricate an agent acknowledgment merely because bytes were placed in its prompt.
Only after the server's ack disposition may normal agent operations proceed.
Crash/lost ack retries preserve the same receipt identity. A stale prior attempt
cannot clear the current continuation hold.

Native provider conversation resume is optional and explicit. A reconstructed
fresh conversation with summary, source and verified outcome must work without
the old provider session key, old worker directory or inherited credentials.
Central supervisor hosting consumes the same public modules with its central
permit adapter, without creating a fake worker identity.

## 5. Required component and integration evidence

- Reader fixtures for all four harnesses: partial records, rotation/truncation,
  unknown format, counter reset, replay, corrected usage and old conversation
  history. Codex/Claude selected product configurations require native accounting;
  other unavailable fields remain explicit and policy-constrained.
- Fake coordinator tests: cursor/outbox atomicity, dropped batch acknowledgments,
  bounded overflow, scoped paths, restart and independent renewal/cancellation.
- Actual tmux with deterministic child: nudge/human-control exclusion, uncertain
  injection, stop ownership and no input after suspension begins.
- Suspension crashes at every journal/stop/snapshot/upload/receipt/release boundary,
  source size/format refusal, expired agent tokens and reconciliation of stale
  generations. Compute release must not imply protected disk release.
- Fake continuation grants: same-attempt replay, fresh attempt on another node,
  missing artifacts, stale ack, failed bootstrap and native-resume unavailability.

Normal unit tests need no model credentials, Discord, sibling repositories or
live AQ daemon. INT-03 separately qualifies actual harnesses, AQ/graph/Git source
owners, browser/CLI and cross-node wake using exact published component artifacts.

## Revision-pinned implementation evidence

The [bounded v1 reference inventory](../../../agent-q/docs/research/2026-10-30-v1-implementation-reference-map.md) records implementation symbols, selected test obligations and per-file hashes at revision `9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`. It distinguishes preserved behavior from remote-authority and retention adaptations. This evidence informs the owning v2 tasks; replacement coverage and product qualification remain pending.
