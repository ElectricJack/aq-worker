# Phase output relay integration

The native phase host now has one registry-owned output reader. Capture starts at genuine local Running before a network Started acknowledgment. Terminal registration requires the exact persisted acknowledgment and host identity; registration is read-only and refuses input-enabled policy. Uploads preserve pending bytes and sequence ranges across uncertain responses. No network request holds host, journal or tail locks.

The actual worker process probe coordinates a human Watch, uploads real periodic tmux output, physically stops the host, persists both lifecycle acknowledgments, and verifies the dashboard receives the resulting literal tail. See [cross-repository integration](../../../agent-q/docs/implementation/AQ-08-terminal-tail-integration/README.md). The complete process/browser run passed in 9.48 seconds.

Main source is `4a02832` (runtime, tests and probe). The actual browser used runtime `60607b9`; later changes add host tests and simplify Drop/test lock scopes without changing runtime behavior. Five bounded exchange tests and two actual-host cases cover packet/replay bounds, ACK validation, pre-ACK refusal, exclusive reader ownership, cleanup, and refusal of input-enabled policy.

This is a reusable native API plus an opt-in process probe. The ordinary production worker command does not yet launch real native harnesses. There is no input, complete screen snapshot, journal recovery/adoption, phase renewal or settlement in this slice. Output remains bounded and volatile.

## Explicit output-only Watch polling candidate

Worker source `f33d8ec` introduces the explicit additive `/v1/terminal-relay/output-poll` path through `RelayHttp::output_poll`, using unchanged `RelayPoll`/`Envelope` wire values. It requires nonblocking `hold_ms=0` and `active_controllers=0`, the exact remote peer/capability, a monotonic transport cursor and Watch-only responses. Full `/poll` retains its existing behavior; a zero controller hint is never reinterpreted as an input-policy boundary. Paired server source owns Watch-only selection before its limit/cursor and fresh node/terminal authorization.

Each live relay retains a separate transport poll cursor and at most32 matching-terminal Watch subscriptions, pruning expiry using a fresh clock after the response. Other terminals' valid Watch metadata is left to their independently polling handles. Unknown/control/input/capture/fence effects cause refusal with no cursor advancement or acknowledgment. Watch metadata is neither a grant nor host authority; the server remains authoritative for output acceptance.

The ordinary publisher now owns independent upload and poll tasks, mutexes and transport lanes. A held or409-refused upload cannot starve polling. The original exact pending upload remains retained across polls/errors; its output-through watermark and upload cursor never come from the poll cursor. Drop aborts both disposable tasks, leaving physical stop ownership unchanged. No Bridge execution, control acknowledgment, snapshot capture or input path is added.

This unpublished candidate advances the worker package to0.5.1 and bridge to0.1.3 with exact internal pins; disk FORMAT12 and every frozen protocol/component archive remain unchanged. Source review and focused synthetic-HTTP checks qualify the component join, not live human Watch delivery or physical/provider execution. See the [candidate qualification](evidence/output-only-poll-f33d8ec/manifest.json) for exact source, checks and binary pin. No publication is claimed.
