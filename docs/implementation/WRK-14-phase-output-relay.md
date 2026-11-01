# Phase output relay integration

The native phase host now has one registry-owned output reader. Capture starts at genuine local Running before a network Started acknowledgment. Terminal registration requires the exact persisted acknowledgment and host identity; registration is read-only and refuses input-enabled policy. Uploads preserve pending bytes and sequence ranges across uncertain responses. No network request holds host, journal or tail locks.

The actual worker process probe coordinates a human Watch, uploads real periodic tmux output, physically stops the host, persists both lifecycle acknowledgments, and verifies the dashboard receives the resulting literal tail. See [cross-repository integration](../../../agent-q/docs/implementation/AQ-08-terminal-tail-integration/README.md). The complete process/browser run passed in 9.48 seconds.

Main source is `4a02832` (runtime, tests and probe). The actual browser used runtime `60607b9`; later changes add host tests and simplify Drop/test lock scopes without changing runtime behavior. Five bounded exchange tests and two actual-host cases cover packet/replay bounds, ACK validation, pre-ACK refusal, exclusive reader ownership, cleanup, and refusal of input-enabled policy.

This is a reusable native API plus an opt-in process probe. The ordinary production worker command does not yet launch real native harnesses. There is no input, complete screen snapshot, journal recovery/adoption, phase renewal or settlement in this slice. Output remains bounded and volatile.
