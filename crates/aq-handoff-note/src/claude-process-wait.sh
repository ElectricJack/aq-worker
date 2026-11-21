# A foreground shell running this exact loop is itself a match for pgrep -f.
# Reject only the unbounded sleep loop, before its first iteration.
[ -x /usr/bin/python3 ] || exit 0
exec /usr/bin/python3 -I -c '
import json
import re
import sys

command = json.load(sys.stdin).get("tool_input", {}).get("command", "")
if not isinstance(command, str) or "pgrep" not in command:
    sys.exit(0)
loop = re.match(
    r"^\s*(?:until\s+!\s+|while\s+)pgrep\s+-f\s+"
    r"(?P<quote>[\x22\x27])(?P<pattern>[A-Za-z0-9/ _:-]+)(?P=quote)"
    r"\s*(?:>\s*/dev/null\s*)?[;\n]\s*do\s+sleep\s+\d+(?:\.\d+)?\s*[;\n]\s*done\b",
    command,
)
if loop:
    print("This process-name loop matches its own shell and cannot finish. "
          "Read the original background task output and exit status, or wait on its process ID. "
          "If the original check has exited, continue with its recorded result.", file=sys.stderr)
    sys.exit(2)
'
