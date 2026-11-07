#!/usr/bin/env bash
# Build the static (musl) v2 CLI `aqv2` from a pinned aq-server commit, the
# way aq-server's CI `musl` job and release.yml build it, inside the pinned
# builder image from pins.json. Used until aq-server publishes the
# `aqv2-x86_64-linux` release asset; the result is the `aqv2` public input.
#
# Usage: build-aqv2.sh AQ_SERVER_CHECKOUT OUTPUT_DIR
#   Reads inputs.aqv2.source_revision from pins.json, exports that commit with
#   `git archive` (committed files only), builds it in a fresh container with
#   a private target directory and writes OUTPUT_DIR/aqv2 and its SHA-256.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
server=${1:?aq-server checkout} out=${2:?output directory}
pins=$here/pins.json
revision=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["inputs"]["aqv2"]["source_revision"])' "$pins")
builder=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["builder"])' "$pins")
[ ! -e "$out" ] || { echo "output directory must be fresh" >&2; exit 2; }
mkdir -p "$out/src" "$out/target"
git -C "$server" archive --format=tar "$revision" | tar -x -C "$out/src"
docker run --rm --network host -u 0 -v "$out/src:/src:ro" -v "$out/target:/target" -w /src \
  -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 -e CC_x86_64_unknown_linux_musl=musl-gcc -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$builder" sh -ec '
    apt-get update -q >/dev/null && apt-get install -y -q --no-install-recommends musl-tools file >/dev/null
    rustup target add x86_64-unknown-linux-musl >/dev/null
    cargo build --release --locked -p aqv2 --target x86_64-unknown-linux-musl
    file /target/x86_64-unknown-linux-musl/release/aqv2 | grep -q "statically linked\|static-pie linked"
    chown -R "$HOST_UID:$HOST_GID" /target'
install -m 0755 "$out/target/x86_64-unknown-linux-musl/release/aqv2" "$out/aqv2"
(cd "$out" && sha256sum aqv2 | tee aqv2.sha256)
rm -rf "$out/src" "$out/target"
echo "aq-server $revision"
pinned=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["inputs"]["aqv2"]["sha256"])' "$pins")
[ "$(cut -d' ' -f1 "$out/aqv2.sha256")" = "$pinned" ] || { echo "aqv2 differs from the pinned SHA-256 $pinned" >&2; exit 1; }
echo "aqv2 matches pins.json"
