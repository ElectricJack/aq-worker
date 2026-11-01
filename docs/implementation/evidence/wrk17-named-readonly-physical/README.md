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
