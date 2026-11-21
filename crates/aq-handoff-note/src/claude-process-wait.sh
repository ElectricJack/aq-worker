# A foreground shell running this exact loop is itself a match for pgrep -f.
[ -x /usr/bin/python3 ] || exit 0
exec /usr/bin/python3 -I - 3<&0 <<'PY'
import json
import os
import re
import sys

with os.fdopen(3) as source:
    command = json.load(source).get("tool_input", {}).get("command", "")
if not isinstance(command, str) or "pgrep" not in command:
    sys.exit(0)
loop = re.compile(
    r"""\s*(?:until\s+!\s+|while\s+)pgrep\s+-f\s+(?P<quote>["'])(?P<pattern>[A-Za-z0-9/ _:-]+)(?P=quote)"""
    r"\s*(?:>\s*/dev/null\s*)?[;\n]\s*do\s+sleep\s+\d+(?:\.\d+)?\s*[;\n]\s*done\b"
)
# Cross simple command separators, preserving quoted strings and escapes. Stop
# on substitutions, here-documents or other constructs we cannot identify.
fragment = re.compile(r"""[;\n]|[^"'\\;\n$\x60#(<)]+|'[^']*'|"(?:\\[\s\S]|[^"\\$\x60])*"|\\[\s\S]|\$(?!\()|<(?!<)|\#[^\n]*""")
offset = 0
at_start = True
while offset < len(command):
    if at_start and loop.match(command, offset):
        print("This process-name loop matches its own shell and cannot finish. "
              "Run any preceding edits separately. Read the original background task output "
              "and exit status, or wait on its process ID.", file=sys.stderr)
        sys.exit(2)
    part = fragment.match(command, offset)
    if part is None:
        break
    at_start = part.group() in (";", "\n")
    offset = part.end()
PY
