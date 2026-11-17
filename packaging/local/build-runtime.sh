#!/usr/bin/env bash
# Build the worker's host programs from this checkout into one runtime
# directory: aq-worker, aq-sandbox-driver (the same bytes as aq-worker; its
# SHA-256 is pinned by the server's definitions and the worker's native.json),
# aq-session-host, aq-agent and aq-git.
#
# Usage: packaging/local/build-runtime.sh OUTPUT_DIR [--aq-git-checkout DIR | --aq-git BINARY]
#
# aq-git comes from a sibling aq-git checkout (built here) or a given binary.
# The build is a host build: the programs run on this machine only.
# CARGO_BUILD_JOBS and CARGO_TARGET_DIR are honoured.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=${1:?usage: build-runtime.sh OUTPUT_DIR [--aq-git-checkout DIR | --aq-git BINARY]}; shift
git_checkout=$root/../aq-git git_binary=
while [ $# -gt 0 ]; do
  case $1 in
    --aq-git-checkout) git_checkout=${2:?}; shift 2 ;;
    --aq-git) git_binary=${2:?}; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
target=${CARGO_TARGET_DIR:-$root/target}
(cd "$root" && cargo build --release --locked -p aq-worker -p aq-session-host -p aq-agent-client --bins)
if [ -z "$git_binary" ]; then
  [ -f "$git_checkout/Cargo.toml" ] || { echo "no aq-git checkout at $git_checkout; pass --aq-git-checkout or --aq-git" >&2; exit 2; }
  # Its own target directory: never share one across repositories.
  (cd "$git_checkout" && CARGO_TARGET_DIR="$git_checkout/target" cargo build --release --locked -p aq-git-cli)
  git_binary=$git_checkout/target/release/aq-git
fi
mkdir -p "$out"
# Install by rename, so a running service keeps the bytes it started with.
put() { install -m 0755 "$1" "$out/.$2.new" && mv "$out/.$2.new" "$out/$2"; }
put "$target/release/aq-worker" aq-worker
put "$target/release/aq-worker" aq-sandbox-driver
put "$target/release/aq-session-host" aq-session-host
put "$target/release/aq-agent" aq-agent
put "$git_binary" aq-git
"$out/aq-worker" --version
(cd "$out" && sha256sum aq-worker aq-sandbox-driver aq-session-host aq-agent aq-git)
