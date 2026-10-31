# Native startup and terminal reader host union

Source `3825480` combines the literal final-payload API, independent guardian-owned
reader expiry and bounded incremental control decoding. Host version is0.1.3;
no0.1.2 package archive was published. [Exact evidence](WRK-08-host-union-qualification.json)
records307 normal workspace tests,39 ignored, plus32 explicitly executed actual
host tests and3 actual bridge tests. All passed, as did strict workspace lint and
formatting. The one merge conflict joined independent test additions.

The startup API seals the descriptor/payload identity in the private configuration,
stages one exact payload and rechecks bytes before native exec. Actual tmux tests
observe exact UTF8/newline/NUL/literal bytes at the owned fixture and refuse a
payload changed after durable launch intent. Full descriptor semantics remain the
runtime consumer's job; the host has no aq-admission implementation dependency.

Reader ownership and incremental decoding retain the prior fixes and their
negative evidence. A stalled consumer loses its exact read client independently
of harness authority; finite packet decoding does not silently discard dense
control output. Full rendering/baseline and native runtime integration remain open.

The workspace test-process sum was128.56seconds, excluding build time; the existing
under-two-minute target is still unmet. Actual host tests took5.50seconds and
bridge tests0.48seconds on this shared machine. Earlier crash-fixture marker
failures remain unresolved historical observations, not removed from evidence.
