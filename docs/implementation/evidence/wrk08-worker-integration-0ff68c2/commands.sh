# Run from the qualified worker checkout. Logs are retained separately.
export CARGO_TARGET_DIR=/home/jkern/dev/aq-worker/target
export CARGO_BUILD_JOBS=2
# At9dcce81:
cargo test --workspace --all-targets --all-features --locked --offline -- --test-threads=2
AQ_GIT_TEST_BINARY=/home/jkern/dev/aq-git/target/debug/aq-git cargo test -p aq-worker --test source --all-features --locked --offline -- --ignored --test-threads=1
cargo test -p aq-session-host --features test-support --test local_tmux --locked --offline -- --ignored --test-threads=2
# After0ff68c2 runtime-pin followup, package regression then controlled host:
cargo test -p aq-worker --all-features --locked --offline -- --test-threads=2
cargo test -p aq-session-host --features test-support --test local_tmux --locked --offline -- --ignored --test-threads=2
cargo test -p aq-terminal-bridge --features test-support --test local_relay --locked --offline -- --ignored --test-threads=2
cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
