#!/usr/bin/env bash
set -euo pipefail
export CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target
export CARGO_INCREMENTAL=0
# Root reported this completed command and exit0; original shell/thread count
# and build-job count were not retained. This is the known command, not an
# invented reconstruction of the missing shell metadata.
cargo test --workspace --all-features --locked --offline
# Direct integration-qualification commands below:
CARGO_BUILD_JOBS=3 cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
CARGO_BUILD_JOBS=3 cargo test -p aq-terminal-bridge --all-features --test local_relay --locked --offline -- --ignored --test-threads=2
