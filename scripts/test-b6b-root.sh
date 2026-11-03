#!/usr/bin/env bash
# Run only B6b's numeric-UID kernel probe, independently of aq-sandbox-os.
set -euo pipefail
probe=$(realpath "$1")
shift
exec docker run --pull=never --rm --network none --cap-drop ALL \
  --cap-add SETUID --cap-add SETGID --cap-add KILL \
  --mount "type=bind,src=$probe,dst=/probe,readonly" \
  --mount type=bind,src=/lib/x86_64-linux-gnu,dst=/lib/x86_64-linux-gnu,readonly \
  --mount type=bind,src=/usr/bin/tmux,dst=/usr/bin/tmux,readonly \
  --entrypoint /probe ubuntu@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3 "$@"
