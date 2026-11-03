#!/usr/bin/env bash
# drill-restart.sh: record the E6a "Windows restart" drill (DEP3).
#
#   drill-restart.sh before <profile> <workers-api-url>   # then restart Windows and log on
#   drill-restart.sh after  <profile> <workers-api-url>   # once WSL is back (the logon task boots it)
#
# Each phase appends a secret-free snapshot to $AQ_DRILL_LOG (default
# ~/.local/state/agent-q/drill-restart.log): uptime, linger, unit states, the
# Windows keepalive task, journal identity/revision/orphans and the server's view
# of the node. `after` fails unless the node id is unchanged, the boot id is new,
# both units are active, and the server reports the node online.
set -euo pipefail
phase="${1:?before|after}" p="${2:?profile}" api="${3:?workers API URL, e.g. http://127.0.0.1:32838/api/workers}"
LOG="${AQ_DRILL_LOG:-${XDG_STATE_HOME:-$HOME/.local/state}/agent-q/drill-restart.log}"
JOURNAL="${XDG_STATE_HOME:-$HOME/.local/state}/agent-q/workers/$p/controller/journal.json"
SNAP="$(dirname "$LOG")/drill-restart.$p.before.json"
install -d -m 700 "$(dirname "$LOG")"

snapshot() {
  python3 - "$JOURNAL" "$api" <<'PY'
import json, sys, urllib.request
journal, api = sys.argv[1:3]
s = json.load(open(journal))["state"]
out = {"node_id": s["identity"]["node_id"], "boot_id": s["identity"]["boot_id"],
       "revision": s["revision"], "orphans": sorted(s.get("orphans", {})),
       "attempts": sorted(s.get("attempts", {}))}
try:
    d = json.load(urllib.request.urlopen(api, timeout=10))
    ws = d.get("workers", d) if isinstance(d, dict) else d
    w = next((w for w in ws if out["node_id"] in (w.get("node_id"), w.get("id"))), None)
    out["server_status"] = w and w.get("status")
except Exception as e:  # recorded, not fatal: the server may still be starting
    out["server_status"] = f"unreachable: {type(e).__name__}"
print(json.dumps(out))
PY
}

{
  echo "=== $phase $(date -Is) profile=$p"
  echo "uptime: $(uptime -s) ($(cut -d' ' -f1 /proc/uptime)s)"
  echo "linger: $(loginctl show-user "$USER" -p Linger --value 2>/dev/null || echo unknown)"
  echo "units: sandbox=$(systemctl --user is-active aq-sandbox-podman.service || true) worker=$(systemctl --user is-active "aq-worker@$p.service" || true)"
  SCHTASKS=/mnt/c/Windows/System32/schtasks.exe
  if [[ -x "$SCHTASKS" ]]; then
    (cd /mnt/c && "$SCHTASKS" /Query /TN "AQ WSL worker keepalive" /FO LIST 2>&1 | tr -d '\r' | grep -E 'Status|Last Run Time|Last Result' | sed 's/^/task: /') || echo "task: query failed"
  fi
} | tee -a "$LOG"

if [[ "$phase" == after ]]; then
  # Give the controller time to come up after the logon task booted the VM.
  for _ in $(seq 1 30); do
    cur="$(snapshot)"; [[ "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["server_status"])' "$cur")" == online ]] && break; sleep 10
  done
  echo "journal/server: $cur" | tee -a "$LOG"
  [[ -f "$SNAP" ]] || { echo "no before snapshot at $SNAP" | tee -a "$LOG"; exit 1; }
  python3 - "$SNAP" "$cur" "$p" <<'PY' | tee -a "$LOG"
import json, subprocess, sys
before = json.load(open(sys.argv[1])); after = json.loads(sys.argv[2]); p = sys.argv[3]
act = lambda u: subprocess.run(["systemctl", "--user", "is-active", u], capture_output=True, text=True).stdout.strip()
checks = {
    "same node_id": before["node_id"] == after["node_id"],
    "new boot_id": before["boot_id"] != after["boot_id"],
    "journal revision advanced": after["revision"] > before["revision"],
    "sandbox daemon active": act("aq-sandbox-podman.service") == "active",
    "worker unit active": act(f"aq-worker@{p}.service") == "active",
    "server reports online": after["server_status"] == "online",
}
for k, v in checks.items():
    print(("PASS " if v else "FAIL ") + k)
print("DRILL " + ("PASS" if all(checks.values()) else "FAIL"))
sys.exit(0 if all(checks.values()) else 1)
PY
else
  snapshot | tee "$SNAP" | sed 's/^/journal\/server: /' | tee -a "$LOG"
  echo "now restart Windows, log on, then run: $0 after $p $api"
fi
