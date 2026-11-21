# A managed task can yield to an in-flight build before its final handoff.
# Preserve the original hook input for the driver only when it should stop.
set -eu
pending=$(mktemp /aq/out/stop.XXXXXX)
trap 'rm -f "$pending"' EXIT
cat > "$pending"
if [ -x /usr/bin/python3 ]; then
    if /usr/bin/python3 -I - "$pending" <<'PY'
import json
import re
import sys

with open(sys.argv[1], "rb") as source:
    hook = json.load(source)
text = hook.get("last_assistant_message") or ""
handoff = re.search(r"(?im)^\s*[- >*_\x60]*changes[*_\x60]*:", text)
blocked = re.search(r"(?im)^\s*[- >*_\x60]*outcome[*_\x60]*:\s*[*_\x60]*blocked\b", text)
if hook.get("hook_event_name") == "Stop" and not (handoff or blocked):
    if not hook.get("stop_hook_active"):
        print(json.dumps({"decision": "block", "reason": "AQ uses your final handoff to stop this session. Finish pending builds and checks and inspect their outcomes before reporting Changes. If you cannot proceed, report Outcome: blocked and the blocker."}))
        sys.exit(10)
    if hook.get("background_tasks"):
        # Let the harness receive the pending task's completion notification.
        sys.exit(10)
PY
    then :
    else
        status=$?
        [ "$status" != 10 ] || exit 0
    fi
fi
mv "$pending" /aq/out/stop.json
