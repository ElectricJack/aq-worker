# aq-session-host 0.1.7 start-request publication evidence

Task: `bold-stone-39`. Linux with tmux 3.4; synthetic local harness, no provider.
Every Cargo command ran inside `aq test docs/implementation/B5a-evidence/test_cargo.py -q`,
using `B5A_COMMAND`, `B5A_LOG`, `B5A_RESULT` and the existing two-job Cargo limit.
The JSON receipts record exact commands and return codes; logs retain full output.

- `atomic.json` / `atomic.log`: three deterministic publication tests passed,
  covering the polling slow writer, no clobber/error cleanup, and two publishers.
- `host.json` / `host.log`: `cargo test --locked -p aq-session-host --all-features`
  passed 73 tests with 59 pre-existing opt-in/fixture ignores. The new physical
  `supervisor_session_starts_under_central_permit` performs 50 sequential fresh
  central launches, startup delivery checks, stops and exact kernel ECHILD proofs.
  Malformed published startup JSON independently stops without creating tmux.
- `clippy.json` / `clippy.log`: all host targets/features passed with `-D warnings`.
  The initial two redundant test-reference diagnostics remain in
  `clippy-before-borrow-fix.*`; production behavior did not change for that fix.
- `legacy-race.json` / `legacy-race.log`: `check_legacy_race.py` reinstated the
  former open-final-before-write semantics in a disposable release extraction.
  The slow-writer regression failed with `partial request was published` (Cargo
  exit 101), as required. The checker passed only after verifying that exact
  failure and restoring the original source bytes. This expected negative is
  deliberately distinct from a passing production-source test.
- `artifact-integrity.json`: the final archive SHA-256/size and every one of its
  1,201 extracted source-file hashes and lengths match the publication receipt.
  Extraction used Python tarfile's `data` filter into a unique `/tmp` directory.
- `artifact-host.json` / `artifact-host.log`: final extracted-release host suite
  passed 73 tests with 59 existing ignores;
  its exact manifest path is in the receipt. Cargo build artifacts were cached in
  the task checkout's target directory; all AQ path dependencies resolve within
  the extracted release. The archive itself and its receipt were never mutated.
- `artifact-host-initial-lock-failure.*`: the first extracted area run failed
  in the existing credential reconciliation lock test after 61 unit tests passed.
  Its `lock`/`reconcile`/`replace` path uses separate file I/O and never calls the
  changed helper. The same test passed unchanged alone (`lock-focused.*`). The
  area suite was rerun unchanged; neither its assertion nor test selection was
  weakened. The initial failure remains visible alongside the final receipt.

The initial workspace host run preceded only the two test-reference lint fixes;
the extracted release run verifies the final source bytes, including another 50
fresh physical central starts. Existing historical 0.1.6 archives/logs are retained.
The host test is the component equivalent of the reported aq-platform startup
failure. The aq-platform checkout and its original physical test are unavailable
in this worker project; aq-platform must qualify its own upgraded F1a composition.

Other checks: `cargo fmt -- --check`, `git diff --check`,
`python3 scripts/package-session-host.py --check`, and final
`git merge origin/main` (already up to date). The initial `cargo fmt --all`
attempt encountered the documented nested-worktree/vendor metadata limitation;
the repository-prescribed workspace-only formatting check succeeds.

Release/admission pins and consumption instructions are in `../F1a-host.md`.
