#!/usr/bin/env bash
# Run a dedicated rootless Podman API service for docker-local sandboxes.
# Everything (config, storage, socket) lives under AQ_SANDBOX_PREFIX and
# $XDG_RUNTIME_DIR/aq-sandbox; the user's own ~/.config/containers is untouched.
# Usage: aq-sandbox-podman.sh install <podman-linux-amd64.tar.gz> | serve | socket
set -euo pipefail
PREFIX="${AQ_SANDBOX_PREFIX:-$HOME/.local/opt/aq-sandbox-podman}"
RUNTIME="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set (systemd user session)}/aq-sandbox"
SOCK="$RUNTIME/podman.sock"
BIN="$PREFIX/podman-linux-amd64/usr/local"
# podman reads policy.json from $HOME/.config only, so the daemon gets its own HOME.
export HOME="$PREFIX/home"
export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$PREFIX/data"
CONF_DIR="$XDG_CONFIG_HOME/containers"
export CONTAINERS_CONF="$CONF_DIR/containers.conf"
export CONTAINERS_STORAGE_CONF="$CONF_DIR/storage.conf"
export CONTAINERS_REGISTRIES_CONF="$CONF_DIR/registries.conf"
export PATH="$BIN/bin:$PATH"

write_config() {
  local c="$CONF_DIR"
  mkdir -p "$c" "$PREFIX/data/storage" "$RUNTIME"
  cat >"$c/containers.conf" <<CONF
[containers]
# Fixed policy is applied per container by the worker; these are daemon defaults.
default_capabilities = []
no_hosts = false
seccomp_profile = "$PREFIX/podman-linux-amd64/etc/containers/seccomp.json"
[engine]
cgroup_manager = "systemd"
events_logger = "file"
helper_binaries_dir = ["$BIN/lib/podman", "$BIN/bin"]
conmon_path = ["$BIN/lib/podman/conmon"]
runtime = "runc"
[engine.runtimes]
runc = ["$BIN/bin/runc"]
[network]
network_backend = "netavark"
default_rootless_network_cmd = "pasta"
CONF
  cat >"$c/storage.conf" <<CONF
[storage]
driver = "overlay"
graphroot = "$PREFIX/data/storage"
runroot = "$RUNTIME/storage"
[storage.options.overlay]
# Required only for single-UID mapping (no newuidmap); harmless otherwise.
ignore_chown_errors = "true"
CONF
  cat >"$c/registries.conf" <<CONF
unqualified-search-registries = ["docker.io"]
short-name-mode = "enforcing"
CONF
  # Templates are pinned by digest and allowlisted by the worker (C3b);
  # signature verification policy is out of scope for this daemon.
  printf '{"default":[{"type":"insecureAcceptAnything"}]}\n' >"$c/policy.json"
}

# Without the setuid newuidmap/newgidmap helpers (Ubuntu package `uidmap`)
# Podman refuses to start once /etc/subuid lists the user. AQ_SANDBOX_SINGLE_UID=1
# hides the ranges in a private mount namespace so Podman falls back to its
# single-UID mapping. Qualification fallback only: every container then runs
# as one host UID, so in-sandbox runner/agent user separation is unavailable.
single_uid() {
  local empty="$PREFIX/empty-subid"
  : >"$empty"
  exec unshare --user --map-current-user --keep-caps --mount sh -c \
    'mount --bind "$1" /etc/subuid && mount --bind "$1" /etc/subgid && shift &&
     exec setpriv --inh-caps=-all --ambient-caps=-all "$@"' sh "$empty" "$@"
}
run() {
  if [ "${AQ_SANDBOX_SINGLE_UID:-0}" = 1 ]; then single_uid "$@"; else exec "$@"; fi
}

case "${1:-}" in
  install)
    tarball="${2:?path to podman-linux-amd64.tar.gz}"
    mkdir -p "$PREFIX"
    tar -xzf "$tarball" -C "$PREFIX"
    write_config
    echo "installed under $PREFIX; socket will be $SOCK" ;;
  serve)
    write_config
    rm -f "$SOCK"
    run podman system service --time=0 "unix://$SOCK" ;;
  podman)
    shift; write_config; run podman "$@" ;;
  socket)
    echo "$SOCK" ;;
  *)
    echo "usage: $0 install <tarball> | serve | podman <args> | socket" >&2; exit 2 ;;
esac
