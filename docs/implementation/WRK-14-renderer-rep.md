# REP after combining: physical oracle disagreement

The first pure model checkpoint715c4b3 is not a physical compatibility claim.
Root's actual tmux3.4 oracle showed `A CSI3b |` produces `AAAA|`, while
`e+U+0301 CSI3b |` produces `é|` with the trailing marker observed. The initial
model retained the base `e` as last_print and would invent three repetitions.

The narrow correction clears REP qualification after any appended zero-width
scalar. A subsequent REP refuses permanently consistent state rather than
silently guessing. Ordinary ASCII REP remains supported. The binary image
preserves this refusal state across restore; it does not regain a stale base.
The profile documentation names the limitation. No frozen archive had been
published by this worktree; the source candidate remains0.1.0.

Locked/offline tests pass16/16, strict all-target/all-feature Clippy and formatting
pass, using the dedicated clean renderer target and CARGO_INCREMENTAL=0.
The retained root oracle and independent finite reproduce.py agree on the first
line of both cases. The local reproduction disables the tmux status line and
records its actual4row pane; parent-oracle trailing blank-line geometry need not
be conflated with REP semantics. It is a synthetic screen oracle, not host
ownership/stopped proof, provider compatibility or a Snapshot producer.

`WRK-14-renderer-rep.json` hashes the source inventory, raw checks, oracle and
reproduction artifacts. Full actual terminal compatibility, especially resize,
Unicode format/bidi and browser rendering, remains separately unqualified.
