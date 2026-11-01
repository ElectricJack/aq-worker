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
cc/bwrap; neither becomes a mandatory production dependency.


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
