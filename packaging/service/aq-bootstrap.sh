#!/usr/bin/env bash
# Downloaded from the operator's private worker endpoint. stdin stays attached
# to the terminal so aq-worker can read the enrollment code without echoing it.
set -euo pipefail
umask 077

origin="${1:?usage: aq-bootstrap.sh https://worker-host [installer options]}"
shift
[[ "$origin" =~ ^https://[a-zA-Z0-9][a-zA-Z0-9.-]*(:[0-9]{1,5})?$ ]] || {
  echo 'Use the HTTPS worker address shown in Add machine.' >&2; exit 2;
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

install_packages() {
  command -v apt-get >/dev/null && command -v sudo >/dev/null &&
    sudo apt-get update && sudo apt-get install -y "$@"
}

if ! command -v python3 >/dev/null || ! command -v git >/dev/null || ! command -v newuidmap >/dev/null || ! command -v tmux >/dev/null; then
  if ! install_packages ca-certificates git python3 tmux uidmap libutempter0; then
    echo 'Install Python 3, Git, uidmap, tmux and libutempter0, then rerun the command.' >&2; exit 3
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
    echo 'Worker bundle checksum did not match. Copy a fresh command from Add machine.' >&2; exit 4;
  }
  mkdir "$work/package"
  python3 -m tarfile --filter data --extract "$work/bundle.tar.gz" "$work/package"
  mv -T -- "$work/package" "$package"
fi

profile=local
slots=1
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in
    --profile) profile="${args[i+1]:-}" ;;
    --slots) slots="${args[i+1]:-}" ;;
  esac
done
[[ "$profile" =~ ^[a-z0-9][a-z0-9-]{0,31}$ && "$slots" =~ ^[1-8]$ ]] || {
  echo 'Choose a valid profile and between one and eight slots.' >&2; exit 2;
}
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/agent-q/service/$profile"
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
for tool in aq-worker aq-session-host aq-agent aq-git aq-sandbox-driver tmux; do
  libraries="$(ldd "$package/$tool" 2>&1 || true)"
  if [[ "$libraries" == *'not found'* ]]; then
    echo "A system library required by $tool is missing." >&2
    grep 'not found' <<<"$libraries" >&2
    exit 3
  fi
done
if [[ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null || true)" != yes ]]; then
  if command -v sudo >/dev/null; then sudo loginctl enable-linger "$(id -un)"; fi
fi
install_args=()
token_file="${AQ_CLAUDE_TOKEN_FILE:-$HOME/.config/aq-secrets/claude-oauth-token}"
if [[ -r "$token_file" ]]; then
  model="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["measurement_model"] or "")' "$package/bootstrap.json")"
  if [[ -n "$model" ]]; then
    install_args+=(--measurement-token-file "$token_file" --measurement-model "$model")
  fi
fi
AQ_SANDBOX_DAEMON_DIR="$package/sandbox-daemon" bash "$package/aq-install.sh" \
  --server "$origin" --bundle "$package" --native-template "$work/native.json" \
  --sandbox-bundle "$package/podman-linux-amd64.tar.gz" --defer-start "${install_args[@]}" "$@"

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
python3 "$work/configure.py" configure "$package" "$config_dir" "$origin" "$podman" "$slots" "$digest"
worker="$HOME/.local/share/agent-q/runtime/aq-worker"
"$worker" service install --profile "$profile"
"$worker" service status --profile "$profile"
echo 'Worker installed. Check Agents → Swarm for its connection.'
