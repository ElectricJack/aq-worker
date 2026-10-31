#!/usr/bin/env bash
set -euo pipefail
cd /home/jkern/dev/aq-worker-wrk14
out="$PWD/target/wrk14-stage1-evidence"
export CARGO_BUILD_JOBS=3
cargo test --workspace --all-features --locked --offline -- --test-threads=2 > "$out/workspace-tests.log" 2>&1
cargo test -p aq-session-host --all-features --test local_tmux --locked --offline -- --ignored --test-threads=2 > "$out/tmux-tests.log" 2>&1
cargo test -p aq-terminal-bridge --all-features --test local_relay --locked --offline -- --ignored --test-threads=2 > "$out/local-relay-tests.log" 2>&1
cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings > "$out/clippy.log" 2>&1
cargo fmt --all -- --check > "$out/format.log" 2>&1
printf '0\n' > "$out/exit-code.txt"
