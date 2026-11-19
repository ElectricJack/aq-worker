#!/usr/bin/env bash
# Downloaded from the operator's private worker endpoint. stdin stays attached
# to the terminal so aq-worker can read the enrollment code without echoing it.
set -euo pipefail
umask 077

origin="${1:?usage: aq-bootstrap.sh https://worker-host [installer options]}"
shift
[[ "$origin" =~ ^https://[a-zA-Z0-9][a-zA-Z0-9.-]*(:[0-9]{1,5})?$ ]] || {
  echo 'Use the HTTPS worker address shown in Add or update machine.' >&2; exit 2;
}
[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 && "$(id -u)" != 0 ]] || {
  echo 'Run this as your normal user in x86-64 Linux or Ubuntu WSL.' >&2; exit 2;
}
if ! systemctl --user show-environment >/dev/null 2>&1; then
  echo 'Start a systemd-enabled WSL distro, then run this command again. Set [boot] systemd=true in /etc/wsl.conf and restart WSL if needed.' >&2
  exit 3
fi
[[ -n "${XDG_RUNTIME_DIR:-}" ]] || { echo 'The systemd user session has no runtime directory.' >&2; exit 3; }
for arg in "$@"; do
  case "$arg" in
    --server|--bundle|--native-template|--repositories|--sandbox-bundle|--defer-start)
      echo "The downloaded bundle sets $arg." >&2; exit 2 ;;
  esac
done

# Substituted by prepare-worker-install.py before the dashboard pins this script.
digest='@AQ_ARCHIVE_SHA256@'
[[ "$digest" =~ ^[a-f0-9]{64}$ ]] || { echo 'This installer has not been packaged.' >&2; exit 2; }
install_root="${XDG_DATA_HOME:-$HOME/.local/share}/agent-q/installer"
mkdir -p "$install_root"
work="$(mktemp -d "$install_root/stage.XXXXXXXX")"
trap 'rm -rf -- "$work"' EXIT
package="$install_root/$digest"
# The bootstrap carries configuration fixes independently of the pinned runtime bundle.
cat > "$work/configure.py" <<'AQ_INSTALL_CONFIG'
@AQ_INSTALL_CONFIG@
AQ_INSTALL_CONFIG

# A connected worker already has its runtime and receiver unit. Control updates
# must remain possible even when native setup validation is what needs diagnosis.
controls_only=0
control_profile=local
control_args=("$@")
for ((i=0; i<${#control_args[@]}; i++)); do
  case "${control_args[i]}" in
    --controls-only) controls_only=1 ;;
    --profile) control_profile="${control_args[i+1]:-}" ;;
  esac
done
if [[ "$controls_only" == 1 ]]; then
  [[ "$control_profile" =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]] || { echo 'Invalid worker profile.' >&2; exit 2; }
  control_config="${XDG_CONFIG_HOME:-$HOME/.config}/agent-q/service/$control_profile"
  python3 "$work/configure.py" upgrade-controls "$package" "$control_config" "$origin"
  exit 0
fi

install_packages() {
  command -v apt-get >/dev/null && command -v sudo >/dev/null &&
    sudo apt-get update && sudo apt-get install -y "$@"
}

if ! command -v python3 >/dev/null || ! command -v git >/dev/null || ! command -v newuidmap >/dev/null || ! command -v tmux >/dev/null || ! command -v iptables >/dev/null; then
  if ! install_packages ca-certificates git python3 tmux uidmap libutempter0 iptables; then
    echo 'Install Python 3, Git, uidmap, tmux, libutempter0 and iptables, then rerun the command.' >&2; exit 3
  fi
fi
if ! awk -F: -v name="$(id -un)" -v uid="$(id -u)" '
  $1 == name || $1 == uid { found[FILENAME] = 1 }
  END { exit !(found[ARGV[1]] && found[ARGV[2]]) }
' /etc/subuid /etc/subgid; then
  echo 'Rootless Podman needs a subordinate UID and GID range for your Linux user. Configure those ranges, then rerun the command.' >&2
  exit 3
fi
if [[ ! -d "$package" ]]; then
  echo 'Downloading the worker bundle.'
  curl --proto '=https' --fail --silent --show-error --retry 2 --connect-timeout 10 \
    --max-time 900 --max-filesize 536870912 "$origin/install/$digest.tar.gz" -o "$work/bundle.tar.gz"
  printf '%s  %s\n' "$digest" "$work/bundle.tar.gz" | sha256sum --check --status || {
    echo 'Worker bundle checksum did not match. Copy a fresh command from Add or update machine.' >&2; exit 4;
  }
  mkdir "$work/package"
  python3 -m tarfile --filter data --extract "$work/bundle.tar.gz" "$work/package"
  mv -T -- "$work/package" "$package"
fi

profile=local
nodes=1
explicit_nodes=0
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in
    --profile) profile="${args[i+1]:-}" ;;
    --nodes|--slots) nodes="${args[i+1]:-}"; explicit_nodes=1 ;;
  esac
done
[[ "$profile" =~ ^[a-z0-9][a-z0-9-]{0,31}$ && "$nodes" =~ ^[1-8]$ ]] || {
  echo 'Choose a valid profile and between one and eight nodes.' >&2; exit 2;
}
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/agent-q/service/$profile"
if [[ -f "$config_dir/service.toml" && "$explicit_nodes" == 0 ]]; then
  nodes="$(python3 -c 'import sys,tomllib; print((lambda c: c.get("nodes", c.get("slots")))(tomllib.load(open(sys.argv[1], "rb"))))' "$config_dir/service.toml")"
  [[ "$nodes" =~ ^[1-8]$ ]] || { echo 'The existing worker slot count is invalid.' >&2; exit 2; }
  args+=(--nodes "$nodes")
fi
podman="$HOME/.local/opt/aq-sandbox-podman/podman-linux-amd64/usr/local/bin/podman"
socket="$XDG_RUNTIME_DIR/aq-sandbox/podman.sock"
# Validate runtime pins and resolve host-specific paths before consuming a code.
python3 "$work/configure.py" prepare "$package" "$work" "$origin" "$socket" "$config_dir" "$digest"
tmux_libraries="$(ldd "$package/tmux" 2>&1 || true)"
# Ubuntu 26.04's tmux package dropped the utmp library required by the bundled binary.
if grep -Eq '^[[:space:]]*libutempter\.so\.0[[:space:]]+=>[[:space:]]+not found' <<<"$tmux_libraries"; then
  echo 'Installing the compatibility library required by the bundled tmux.'
  if ! install_packages libutempter0; then
    echo 'Install libutempter0, then rerun the command.' >&2; exit 3
  fi
fi
for tool in aq-worker aq-session-host aq-agent aq-git tmux; do
  libraries="$(ldd "$package/$tool" 2>&1 || true)"
  if [[ "$libraries" == *'not found'* ]]; then
    echo "A system library required by $tool is missing." >&2
    grep 'not found' <<<"$libraries" >&2
    exit 3
  fi
done
# A Claude subscription token file already on this machine enables manual Claude sessions at
# install; otherwise the token arrives later through Push Claude token.
configure_args=()
token_file="${AQ_CLAUDE_TOKEN_FILE:-$HOME/.config/aq-secrets/claude-oauth-token}"
if [[ "$token_file" == /* && -r "$token_file" ]]; then
  configure_args+=("$token_file")
fi
AQ_SANDBOX_DAEMON_DIR="$package/sandbox-daemon" bash "$package/aq-install.sh" \
  --server "$origin" --bundle "$package" --native-template "$work/native.json" \
  --sandbox-bundle "$package/podman-linux-amd64.tar.gz" --defer-start "$@"

echo 'Preparing the pinned sandbox image.'
for ((retry=0; retry<30; retry++)); do
  [[ ! -S "$socket" ]] || break
  sleep 1
done
[[ -S "$socket" ]] || { echo 'The sandbox service did not start. Check journalctl --user -u aq-sandbox-podman.' >&2; exit 5; }
image="$(cat "$work/image")"
if ! "$podman" --remote --url "unix://$socket" image exists "$image"; then
  "$podman" --remote --url "unix://$socket" pull "$image"
fi
python3 "$work/configure.py" configure "$package" "$config_dir" "$origin" "$podman" "$nodes" "$digest" "${configure_args[@]}"
worker="$HOME/.local/share/agent-q/runtime/aq-worker"
"$worker" service install --profile "$profile"
"$worker" service status --profile "$profile"
# The service must now be running the worker that was just installed, not one
# started from the files it replaced.
unit="aq-node@$profile.service"
running=""
for ((retry=0; retry<30; retry++)); do
  pid="$(systemctl --user show "$unit" --property=MainPID --value 2>/dev/null || true)"
  if [[ "$pid" =~ ^[1-9][0-9]*$ ]]; then
    running="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
    [[ "$running" == "$worker" ]] && break
  fi
  sleep 1
done
if [[ "$running" != "$worker" ]]; then
  echo "The worker service is not running the installed worker (it runs: ${running:-nothing})." >&2
  echo "Check: journalctl --user -u $unit -n 40" >&2
  exit 5
fi
echo "Worker running: $("$worker" --version)."
# No second worker service process may be running on this machine: one started
# from replaced files, or another installation, would answer for it with old
# code. The terminal companions also run aq-worker (other subcommands, from
# their own copy); `install-sync` below moves them to this bundle.
others=()
if command -v pgrep >/dev/null; then
  for other in $(pgrep -u "$(id -u)" -x aq-worker || true); do
    exe="$(readlink "/proc/$other/exe" 2>/dev/null || true)"
    mapfile -d '' -t argv < "/proc/$other/cmdline" 2>/dev/null || argv=()
    if [[ "${argv[1]:-}" == run && "$exe" != "$worker" ]]; then
      others+=("$other  ${exe:-unknown}  started $(ps -o lstart= -p "$other" 2>/dev/null || true)")
    fi
  done
fi
if (( ${#others[@]} )); then
  echo 'Another AQ worker is still running on this machine, not the one just installed:' >&2
  printf '  pid %s\n' "${others[@]}" >&2
  echo 'Worker services on this machine:' >&2
  systemctl --user list-units --no-legend --plain 'aq-*' >&2 || true
  echo 'Stop that worker, then run this command again. Send this list to whoever runs the AQ server if it is not clear what it is.' >&2
  exit 5
fi
python3 "$work/configure.py" install-sync "$package" "$config_dir"
echo 'Checking that this machine can start an agent sandbox.'
python3 "$work/configure.py" preflight "$package" "$config_dir" "$podman"
echo 'Worker installed. Check Agents → Swarm for its connection.'
