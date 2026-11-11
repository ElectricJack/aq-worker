#!/usr/bin/env bash
# One-script install of the aq-worker systemd user service (spec 0007 section 7.3).
# Linux and WSL2 only. The single-use enrollment code (and the Access client id and
# secret with --cf-service-token) is read by `aq-worker service enroll` itself from the
# terminal or from stdin; this script never reads, stores or forwards a secret in argv.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: aq-install.sh --server URL --bundle DIR [--profile NAME=local] [--slots N=1] [--name NAME]
                     [--machine-key KEY] [--cf-service-token]
                     [--native-template FILE=DIR/native.json] [--repositories FILE=DIR/repositories.json]
                     [--measurement-token-file FILE --measurement-model MODEL]
                     [--sandbox-bundle podman-linux-amd64.tar.gz] [--reconfigure] [--defer-start]
USAGE
  exit 2
}

KEY_GIVEN=0 RECONFIGURE=0 DEFER_START=0
SERVER="" BUNDLE="" PROFILE="local" SLOTS="1" NAME="" MACHINE_KEY="" CF=0
TEMPLATE="" REPOS="" MTOKEN="" MMODEL="" SANDBOX_BUNDLE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cf-service-token) CF=1; shift; continue ;;
    --reconfigure) RECONFIGURE=1; shift; continue ;;
    --defer-start) DEFER_START=1; shift; continue ;;
    --server|--bundle|--profile|--slots|--name|--machine-key|--native-template|--repositories|\
    --measurement-token-file|--measurement-model|--sandbox-bundle)
      [[ $# -ge 2 ]] || usage ;;
    *) usage ;;
  esac
  case "$1" in
    --server) SERVER="$2" ;;
    --bundle) BUNDLE="$2" ;;
    --profile) PROFILE="$2" ;;
    --slots) SLOTS="$2" ;;
    --name) NAME="$2" ;;
    --machine-key) MACHINE_KEY="$2"; KEY_GIVEN=1 ;;
    --native-template) TEMPLATE="$2" ;;
    --repositories) REPOS="$2" ;;
    --measurement-token-file) MTOKEN="$2" ;;
    --measurement-model) MMODEL="$2" ;;
    --sandbox-bundle) SANDBOX_BUNDLE="$2" ;;
  esac
  shift 2
done
[[ -n "$SERVER" && -n "$BUNDLE" ]] || usage
[[ -z "$MTOKEN" || -n "$MMODEL" ]] || { echo "--measurement-token-file needs --measurement-model." >&2; exit 2; }
[[ -z "$MMODEL" || -n "$MTOKEN" ]] || { echo "--measurement-model needs --measurement-token-file." >&2; exit 2; }
[[ -d "$BUNDLE" ]] || { echo "bundle directory $BUNDLE not found." >&2; exit 2; }
[[ "$SLOTS" =~ ^[0-9]+$ ]] || { echo "--slots must be a number." >&2; exit 2; }
# The service config's rule (aq-node-service valid_profile).
[[ "$PROFILE" =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]] || { echo "invalid --profile: lower-case letters, digits and '-', at most 32 characters." >&2; exit 2; }
TEMPLATE="${TEMPLATE:-$BUNDLE/native.json}"
REPOS="${REPOS:-$BUNDLE/repositories.json}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SANDBOX_DIR="${AQ_SANDBOX_DAEMON_DIR:-$ROOT/scripts/sandbox-daemon}"
RUNTIME_DIR="$HOME/.local/share/agent-q/runtime"
USER_NAME="${USER:-$(id -un)}"
LINK="$HOME/.local/bin/aq-worker"
PROFILE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/agent-q/service/$PROFILE"

# 1. Preflight.
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "Only Linux and WSL2 are supported." >&2
  exit 2
fi
if [[ "$KEY_GIVEN" == 1 && ! "$MACHINE_KEY" =~ ^[a-z0-9][a-z0-9-]{0,62}$ ]]; then
  echo "invalid --machine-key: use lower-case letters, digits and '-', at most 63 characters, starting with a letter or digit." >&2
  exit 2
fi
OSRELEASE="${AQ_OSRELEASE_FILE:-/proc/sys/kernel/osrelease}"
IS_WSL=0
if [[ -r "$OSRELEASE" ]] && grep -qi microsoft "$OSRELEASE"; then IS_WSL=1; fi
PID1="$(tr -d ' \n' < "${AQ_PROC1_COMM_FILE:-/proc/1/comm}" 2>/dev/null || true)"
if [[ "$IS_WSL" == 1 && "$PID1" != "systemd" ]]; then
  cat >&2 <<'WSL'
systemd is not running in this WSL distro. Put this in /etc/wsl.conf:

  [boot]
  systemd=true

then run `wsl --shutdown` once from Windows and start the distro again.
WSL
  exit 3
fi
systemctl --user show-environment >/dev/null || {
  echo "The systemd user manager is not available (systemctl --user show-environment failed)." >&2
  exit 3
}
if [[ "$(loginctl show-user "$USER_NAME" -p Linger --value 2>/dev/null || true)" != "yes" ]]; then
  echo "Lingering is off, so the service would stop at logout. Run: sudo loginctl enable-linger $USER_NAME"
fi
if [[ ( -e "$LINK" || -L "$LINK" ) && "$(readlink "$LINK" || true)" != "$RUNTIME_DIR/aq-worker" ]]; then
  echo "$LINK exists and is not the link this script installs; move it away and run again." >&2
  exit 2
fi
if systemctl --user is-active --quiet aq-worker@local.service; then
  echo "warning: the container worker aq-worker@local is running too. Two workers on one machine compete for it; stop it with: systemctl --user disable --now aq-worker@local" >&2
fi

# 2. Sandbox runtime.
if [[ ! -S "${XDG_RUNTIME_DIR:-/nonexistent}/aq-sandbox/podman.sock" ]]; then
  if [[ -n "$SANDBOX_BUNDLE" ]]; then
    "$SANDBOX_DIR/aq-sandbox-podman.sh" install "$SANDBOX_BUNDLE"
    install -D -m 0755 "$SANDBOX_DIR/aq-sandbox-podman.sh" "$HOME/.local/bin/aq-sandbox-podman.sh"
    install -D -m 0644 "$SANDBOX_DIR/aq-sandbox-podman.service" "$HOME/.config/systemd/user/aq-sandbox-podman.service"
    systemctl --user daemon-reload
    systemctl --user enable --now aq-sandbox-podman.service
  else
    echo "The sandbox socket \$XDG_RUNTIME_DIR/aq-sandbox/podman.sock is missing. Re-run with --sandbox-bundle podman-linux-amd64.tar.gz to install the sandbox runtime." >&2
    exit 4
  fi
fi

# 3. Binaries.
for f in aq-worker aq-session-host aq-agent aq-git aq-sandbox-driver; do
  [[ -f "$BUNDLE/$f" ]] || { echo "bundle is missing $f." >&2; exit 2; }
done
[[ -f "$TEMPLATE" ]] || { echo "native template $TEMPLATE not found." >&2; exit 2; }
[[ -f "$REPOS" ]] || { echo "repositories file $REPOS not found." >&2; exit 2; }
for f in aq-worker aq-session-host aq-agent aq-git aq-sandbox-driver; do
  install -D -m 0755 "$BUNDLE/$f" "$RUNTIME_DIR/$f"
done
mkdir -p "$HOME/.local/bin"
ln -sfn "$RUNTIME_DIR/aq-worker" "$LINK"
WORKER="$RUNTIME_DIR/aq-worker"

# 4. Enroll. stdin and the terminal go straight to aq-worker, which prompts for the code.
# An enrolled profile keeps its enrollment (an upgrade); an unfinished one resumes its claim.
if [[ -f "$PROFILE_DIR/service.toml" && ! -f "$PROFILE_DIR/enroll-pending.toml" && "$RECONFIGURE" == 0 ]]; then
  echo "Profile $PROFILE is already enrolled; enrollment skipped. Pass --reconfigure to enroll it again (after a revoke)."
else
  ENROLL=("$WORKER" service enroll --profile "$PROFILE" --server "$SERVER" --slots "$SLOTS")
  [[ -n "$NAME" ]] && ENROLL+=(--name "$NAME")
  [[ -n "$MACHINE_KEY" ]] && ENROLL+=(--machine-key "$MACHINE_KEY")
  [[ "$CF" == 1 ]] && ENROLL+=(--cf-service-token)
  [[ "$RECONFIGURE" == 1 ]] && ENROLL+=(--reconfigure)
  ENROLL+=(--native-template "$TEMPLATE" --aq-git "$RUNTIME_DIR/aq-git" --repositories "$REPOS")
  [[ -n "$MTOKEN" ]] && ENROLL+=(--measurement-token-file "$MTOKEN" --measurement-model "$MMODEL")
  "${ENROLL[@]}"
fi

# The hosted installer prepares the image before starting the service.
if [[ "$DEFER_START" == 1 ]]; then exit 0; fi

# 5. Install and start (or restart, so a new enrollment takes effect).
"$WORKER" service install --profile "$PROFILE"

# 6. Measure. It holds the profile's measurement lock: the service skips its own meanwhile.
if [[ -n "$MTOKEN" ]]; then
  "$WORKER" service measure --profile "$PROFILE"
fi

# 7. Windows host load: print only, the owner has not approved installing the probe.
if [[ "$IS_WSL" == 1 && ! -e /mnt/c/ProgramData/agent-q/host-load.json ]]; then
  cat <<'PROBE'

Windows host load is not reported yet. To install the probe, run this from Windows
PowerShell in the repository (it is not run for you):

  powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows\aq-host-load.ps1 -Install -StartNow
PROBE
fi

# 8. Status.
"$WORKER" service status --profile "$PROFILE"
echo
echo "This machine is allowed automatically on credentials and projects that allow enrolled machines (allow_enrolled_nodes). Start, stop, drain or revoke it from its pane on the Swarm page."
