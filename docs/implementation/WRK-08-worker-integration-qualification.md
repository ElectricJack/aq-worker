# Native journal and observed host integration

Source `0ff68c27eab628f1c4f390ac89f31ba853dc2ce5` combines the reviewed
observed terminal baseline/sealed-exec host with the native no-launch journal.
The journal retains both original installed runtime and qualification identities.
Independent read-only review of the journal found no blocking issue within this
scope. Exact source/log hashes and commands are in the
[evidence inventory](evidence/wrk08-worker-integration-0ff68c2/artifact-sha256.json).

The combined workspace at `9dcce81` passed 371 tests, with 43 explicit ignores.
All three actual aq-git cases passed using rebuilt source `2035654`; they cover
workspace/checkpoint/export, cold download and cancellation, and lost finalize
and report responses. These use actual Git plus fixture/synthetic coordinator
peers, not a native daemon/model execution. The final runtime-pin followup passed
122 worker-package tests with three actual-Git cases excluded from that command.
Strict workspace all-target/all-feature Clippy and formatting passed on the final
source. Earlier source-specific evidence remains attributed to its original
checkpoint.

Actual tmux qualification passed 36 cases under controlled lower load, and the
actual terminal bridge passed three. The initial concurrent host run passed 35
and failed one: the 1500ms bootstrap authority expired before the intended fault
injection while workspace storage/crash tests and builds ran. Its negative log
is retained. The controlled rerun followed completion of the root storage suite
and worker build, with the same deadlines and two test threads. Other agents
remained active. No deadline was widened, and no saturated-start reliability or
isolated-machine latency claim follows. Counts exclude the nested child-process
probe already covered by its parent test.

This checkpoint supplies component building blocks, not an advertised native
runtime. Durable downloads/material/Prepare, grant/renewal, physical launch and
stop callbacks, result/release, and granted-without-Started recovery remain open.
Restart only permits historical inspection and stop-and-preserve. Logical disk
reservation and injected durability failures do not qualify physical ENOSPC or
hardware power loss. No live provider/model, deployment or publication ran.
