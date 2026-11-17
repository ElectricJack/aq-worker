#!/usr/bin/env bash
# Make sure the dedicated rootless sandbox daemon (Podman API) is installed and
# running for this user. Idempotent: an existing, answering socket is left alone.
#
# Usage: packaging/local/sandbox-daemon.sh [--cache DIR]
#
# Installs the pinned static Podman distribution with
# scripts/sandbox-daemon/aq-sandbox-podman.sh and the user unit
# aq-sandbox-podman.service (the same files packaging/service/aq-install.sh
# installs). Prints the socket path on success.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
PODMAN_VERSION=5.8.8
PODMAN_URL=https://github.com/mgoltzsche/podman-static/releases/download/v$PODMAN_VERSION/podman-linux-amd64.tar.gz
PODMAN_SHA256=e0e12f9de61bdb054d19a77df0e84ddd51cadfee13688e0fb607d61b6833e8cd
cache=${XDG_CACHE_HOME:-$HOME/.cache}/agent-q
while [ $# -gt 0 ]; do
  case $1 in
    --cache) cache=${2:?--cache DIR}; shift 2 ;;
    *) echo "usage: $0 [--cache DIR]" >&2; exit 2 ;;
  esac
done
: "${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set (a systemd user session; on WSL2 enable systemd in /etc/wsl.conf)}"
sock=$XDG_RUNTIME_DIR/aq-sandbox/podman.sock
prefix=${AQ_SANDBOX_PREFIX:-$HOME/.local/opt/aq-sandbox-podman}
answers() { curl -fsS --max-time 5 --unix-socket "$sock" http://d/_ping >/dev/null 2>&1; }

if answers; then echo "$sock"; exit 0; fi
systemctl --user show-environment >/dev/null 2>&1 \
  || { echo "systemd --user is not running; the sandbox daemon is a user service" >&2; exit 4; }
[ "$(stat -fc %T /sys/fs/cgroup)" = cgroup2fs ] \
  || { echo "the sandbox daemon needs the unified cgroup v2 hierarchy" >&2; exit 4; }

if [ ! -x "$prefix/podman-linux-amd64/usr/local/bin/podman" ]; then
  mkdir -p "$cache"
  tarball=$cache/podman-linux-amd64-$PODMAN_VERSION.tar.gz
  if [ ! -s "$tarball" ] || ! echo "$PODMAN_SHA256  $tarball" | sha256sum -c --status; then
    curl -fL --retry 3 -o "$tarball.part" "$PODMAN_URL"
    echo "$PODMAN_SHA256  $tarball.part" | sha256sum -c --status \
      || { echo "downloaded Podman archive does not match the pinned SHA-256" >&2; exit 1; }
    mv "$tarball.part" "$tarball"
  fi
  "$root/scripts/sandbox-daemon/aq-sandbox-podman.sh" install "$tarball" >&2
fi
install -D -m 0755 "$root/scripts/sandbox-daemon/aq-sandbox-podman.sh" "$HOME/.local/bin/aq-sandbox-podman.sh"
unit_dir=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
install -D -m 0644 "$root/scripts/sandbox-daemon/aq-sandbox-podman.service" "$unit_dir/aq-sandbox-podman.service"
# Without the setuid newuidmap/newgidmap helpers (Ubuntu package `uidmap`)
# rootless Podman cannot map a UID range; fall back to the single-UID mode.
if ! command -v newuidmap >/dev/null || ! grep -q "^$(id -un):" /etc/subuid 2>/dev/null; then
  mkdir -p "$unit_dir/aq-sandbox-podman.service.d"
  printf '[Service]\nEnvironment=AQ_SANDBOX_SINGLE_UID=1\n' > "$unit_dir/aq-sandbox-podman.service.d/single-uid.conf"
  echo "newuidmap or a /etc/subuid range is missing: the daemon runs in single-UID mode (install the uidmap package for UID separation)" >&2
fi
systemctl --user daemon-reload
systemctl --user enable --now aq-sandbox-podman.service >&2
for _ in $(seq 1 30); do answers && { echo "$sock"; exit 0; }; sleep 1; done
echo "the sandbox daemon did not answer on $sock; see: journalctl --user -u aq-sandbox-podman.service" >&2
exit 1
