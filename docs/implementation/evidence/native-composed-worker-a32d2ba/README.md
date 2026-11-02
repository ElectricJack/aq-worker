# Composed worker source and pure CLI gate

Source `a32d2baa7f0d35d851e074621abd42ca9dca29d0` merges Root's `03ea9a7` native0.5 result producer and synchronous stop fence with the existing test-support ordinary CLI, finite Responses adapter, and private fixture bootstrap. This is compiled source evidence, not an actual Codex/task execution result.

Checks used the existing `/tmp/aq-worker-artifact-overlap-target`, offline/locked, jobs2, incremental0, debug0. Workspace source entrypoints were touched before compilation to invalidate stale cross-worktree fingerprints; source bytes were not changed by that step.

| Check | Result |
| --- | --- |
| `cargo test -p aq-worker --bin aq-worker --features test-support --locked --offline` | 8 passed, 1 explicitly ignored kernel premise probe; build21.88s, tests0.02s |
| `cargo test -p aq-worker --lib --features test-support finite_responses_fixture --locked --offline` | 3 passed; build17.68s, tests0.00s |
| `cargo clippy -p aq-worker --all-targets --all-features --locked --offline -- -D warnings` | passed20.04s |
| Default bin `production_cli_does_not_expose_fixture_transport_flags` | 1 passed; build11.70s |
| `cargo build -p aq-worker --bin aq-worker --features test-support --locked --offline` | passed23.76s |
| Python bootstrap protocol | 3 passed0.079s; no processes/namespaces |
| Pinned executable ordinary `fixture-responses` vectors/check stdin/stdout | 2 passed, each externally bounded3s; no config/network/harness |

The hostless Python checks cover exact known private files, result-local paths, bounded pre-effect refusal, canonical JSON, and file modes. They do not qualify a kernel mount or runtime authority.

The executable is copied outside Cargo to `/tmp/aq-codex-composed-worker-pin-a32d2ba/aq-worker`, SHA256 `4b573a438bf1bf3b980e8618259017ad57929f03305bacaa5765fdc125e5ec47`, 50,847,424bytes. Pin metadata records worker0.5 source; no old0.4 runtime identity is reused.

No provider, actual Codex, tmux, database, cold Git, canonical, or deployment action ran in these checks. The complete actual task cycle remains a separate composed qualification requiring current artifact recipe, owners, source pins, Watch/output, physical stop, result capture/report, and protected settlement.
