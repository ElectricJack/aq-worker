# Named readonly retained-FD physical probe

The independent `aq-session-host/tests/named_readonly.rs` integration case passed
in 0.70s using the hardened qualification wrapper. Its executable is the checked-in
static C peer, not Codex. It exercises the actual retained-FD observed gate, real
tmux, exact sealed stdin, canonical self path, same-inode symlink helper reexec,
physical Running, and stop with ECHILD. The peer itself waits for its helper before
emitting readiness; the guardian subsequently reaps its owned pane/server.

The JSON observation records exact local pins. The test intentionally uses no
provider, private auth, daemon owner, model or renderer/Snapshot qualification.
This establishes the missing named-FD/self-helper kernel mechanism. It does not
establish Codex tool compatibility or production eligibility.

Build the static source with local `cc -O2 -static` outside the fixture, build the
current host/test executables from source, and select their explicit image/helper/
tmux digest pins using `scripts/qualify-native-loopback.sh`. Only opt-in tests use
cc/bwrap; neither is required by the generic AQ host. The selected Codex Linux
tool profile requires trusted bwrap provisioning as documented below.


### First actual Codex named-profile attempt (negative, 2026-11-01)

The first root-approved paired actual Codex attempt used the hardened private
user/network/PID fixture, the original 20-second Grant, and metadata-only tracing.
It reached genuine physical Running and the first finite local provider request.
The peer checked the exact model/cap (4096), distinct synthetic upstream key,
streaming/stateless request, and absence of hosted web search. The fresh child
capability was present only in its private auth handoff, and absent from that
observed model request; this does not establish source/proc credential isolation.

The attempt failed at the fixture's **three-second terminal-upload gate**, before
it released the finite tool-call response. An initial report attributing the
failure to the first-request notification was incorrect and was corrected after
checking the exact panic location. No tool response was released, no tool command
was requested, and no tool success or final paired cleanup assertions are claimed.
The test result was 0 passed / 1 failed, 24.74 seconds including cold preflight.
The private PID namespace was torn down when the fixture exited.

Source diagnosis: output registration/publication is asynchronous, starts after
an exact Started ACK, and publishes only when retained tail frames/gaps exist.
A first model request does not guarantee a terminal upload before the finite
provider response. The fixture imposed an unsupported ordering. Its corrected
sequence releases the controlled tool response, checks the real tool result and
workspace file, then checks readiness/output while holding the second response.
Every stage uses the remaining original Grant deadline. Stage timeouts request
real-owner stop and record ECHILD/Started/Stopped ACK diagnostics before failing.
No production timeout, lease, sandbox or eligibility rule changes.

Raw diagnostic paths from this failed run: `/tmp/aq-named-owned-codex-process.log`
and `/tmp/aq-named-owned-codex-trace.log`. These are local diagnostic locations,
not durable release artifacts. No final tool or production qualification follows
from this negative result.


### Untraced named Codex tool attempt (negative)

After correcting only fixture ordering/diagnostics, one additional root-approved
untraced attempt ran with identical image/helper/mapper/wrapper, the original
20-second Grant and the same sandbox. It failed in 17.53 seconds total, including
7.04 seconds preflight; the actual execute began with 19.82 seconds remaining.
The [complete bounded diagnostic record](named-codex-bwrap-negative.log) preserves
the real request structure, stage times and cleanup facts. The earlier
[output-gate negative](first-codex-output-gate-negative.log) remains separately
recorded, including its exact failed assertion.

Observed boot-clock sequence (nanoseconds, except the explicitly labeled host
observation):

| Stage | Boot time |
| --- | --- |
| Preflight begin | 216501233489349 |
| Preflight complete | 216508273252854 |
| Execute begin | 216510880410370 |
| Original Grant deadline | 216530702842073 |
| Physical Running observation (milliseconds) | 216511794 |
| Terminal registration | 216512233398937 |
| First terminal upload | 216512516310000 |
| First finite provider request | 216518230869491 |
| Tool-result provider request | 216518295271736 |

The owned scoped transport forwarded two finite requests with exact model/cap,
stream/store policy and distinct synthetic upstream credentials. The actual CLI
returned the exact controlled `call_aq_local` function-call output, but the local
command did **not** succeed: its sandbox helper exited 101 because bubblewrap was
unavailable to its lookup. No workspace proof file or successful tool execution is
claimed. Output registration and three attempted publications receiving HTTP409
(no current Watch) were observed; no UploadAck or accepted output was proved;
the second attempt confirms that the first negative was not an established
output-transport failure.

The failure handler signaled the existing owner without releasing a second model
response. The task joined and persisted actual ECHILD, Started ACK and Stopped ACK.
No lease was extended. The final success-only private-cleanup assertions were not
reached, so this result does not newly qualify that claim.

Read-only source diagnosis: exact Codex 0.166's
[shell environment builder](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/protocol/src/shell_environment.rs)
starts from an empty map for `inherit=none`; the current mapper supplies no fixed
`set.PATH`. Its
[sandbox executable lookup](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/sandboxing/src/bwrap.rs)
requires the helper's PATH, while the
[launcher](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/linux-sandbox/src/launcher.rs)
reports the observed failure when neither a system nor bundled helper is selected.
The trusted fixture contains system bubblewrap 0.9.0 with the required capability
flags. A fixed explicit tool PATH is the proposed narrow mapper correction;
ambient inheritance, mutable helper fallback and sandbox relaxation are not
required. That correction and any future actual launch require separate review.

Fixture-only validation for the corrected ordering: the original-deadline refusal
regression passed (0.01s), fixture compilation passed (15.73s), and strict host plus
worker all-target/all-feature lint passed (7.55s). No production runtime, upstream
source secrecy, real provider or context/model qualification follows from these
local synthetic-peer observations.


### Fixed-PATH actual named Codex tool qualification (positive)

The single additional root-approved untraced attempt **passed**, after importing
only the fixed explicit tool PATH mapper `2f70e14` (local `66422eb`). The existing
hardened wrapper, workspace-write sandbox, network denial, output cap4096 and
original 20-second Grant remained unchanged. Both negative attempts above remain
part of this evidence; this result does not replace or reinterpret them.

The [complete positive diagnostic record](named-codex-fixed-path-positive.log)
and [machine-readable observation and exact source/binary pins](named-codex-fixed-path-positive.json)
record the paired case. Its source/binary bundle digest is
`18c2f81f84a14954fe437f7eb448e2a032f93ace6ed335e8f0c8799443fad7a8`,
computed over the JSON's compact sorted-key `source_binary_bundle` object. This
identifies the local qualification inputs; it is not a published release package
or a rewritten package archive. The JSON pins worker test binary, source-built
host helper, Codex0.166.0, tmux3.4, bwrap0.9.0, wrapper, fixture and mapper sources.

Total test time was **18.27s**, including **7.30s** preflight before admission.
Execute-to-natural-completion took **8.25s**. The execute began at boot time
`216966833505402ns` with deadline `216986633525935ns`; natural physical completion
finished at `216975079151319ns`, leaving **11.55s** of the original Grant.
The controlled first-request to real tool-result-request round trip took
**90.72ms**; this is local synthetic-response timing, not model inference latency.
Physical Running was observed at `216967754ms`, with the exact Started ACK present.
The first finite request arrived at `216974507367253ns`; the actual tool result
request arrived at `216974598086643ns`. No stage obtained a renewed deadline.

Successful assertions prove an actual `exec_command` round trip with controlled
`call_aq_local`: the returned `function_call_output` contains
`aq-local-tool-proof`, and the real owned workspace file
`aq-local-tool-proof.txt` contains exactly those bytes. The fake peer received two
streaming/stateless requests with the exact selected model/cap, no hosted web
search and distinct synthetic upstream credentials. A generated child capability
was different from the upstream key and absent from the observed model inputs.

Terminal registration succeeded, then **three HTTP publication attempts reached
the peer and each received409 `no current Watch`**. The observed request bytes
contained genuine `thread.started`/`turn.started` output without child or upstream
credentials, and the agent remained running while the second model response was
held. No UploadAck was accepted, pending bytes remained retained, and **neither
durable accepted output nor human terminal delivery was proved**.

After releasing the finite final response, the real owner observed tmux-server
root exit0, persisted physical stop with kernel ECHILD, and held exact Started
and Stopped ACKs with each report published once. Matching `codex-home/auth.json`
was removed after physical stop, while retained preparation input ownership
remained. All these success-only assertions were reached in this positive case.

Correction, 2026-11-01: the historical JSON assertion "natural CLI root exit0"
describes `HostObservation.root_status`, which the supervisor records when it
reaps its tmux-server child. It does not prove the Codex pane exited successfully.
Raw JSON, logs and hashes are preserved unchanged. Consumers must treat the
harness process outcome as unknown; the actual tool/file and physical stop
observations retain their stated scope. See `aq-session-host/src/supervisor.rs`
for the root-child ownership and status assignment.

This is isolated local actual-tool/process qualification with synthetic secrets
and a finite loopback Responses peer. The fixture still exposes the original
worker material source inside its shared same-UID filesystem/proc view. It does
not qualify production upstream-key secrecy, per-harness capsule isolation,
external provider behavior, model/context/token bounds or production admission.
The production executor factory remains refused. No further actual launches were
performed after this accepted positive result.
