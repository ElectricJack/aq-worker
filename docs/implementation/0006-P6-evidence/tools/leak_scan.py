"""Leak scan for the phase 6 evidence: agent credentials (the aq agent prefix) and the Claude token.

Usage: leak_scan.py PATH...  Prints counts per file; never prints a match.
The Claude token is read from ~/.config/aq-secrets/claude-oauth-token and only
compared in memory."""
import os, re, sys, json
from pathlib import Path
token = (Path.home() / ".config/aq-secrets/claude-oauth-token").read_bytes().strip()
PREFIX = b"aq" + b"a_"  # spelled apart so this file never matches itself
shaped = re.compile(re.escape(PREFIX) + rb"[A-Za-z0-9_-]{32,}")
total = {"files": 0, "prefix_substring": 0, "credential_shaped": 0, "claude_token": 0}
rows = []
for root in sys.argv[1:]:
    paths = [Path(root)] if Path(root).is_file() else [p for p in Path(root).rglob("*") if p.is_file() and not p.is_symlink()]
    for p in sorted(paths):
        if "/.git/" in str(p):
            continue
        data = p.read_bytes()
        r = {"file": str(p), "prefix_substring": data.count(PREFIX),
             "credential_shaped": len(shaped.findall(data)), "claude_token": data.count(token) if token else 0}
        total["files"] += 1
        for k in ("prefix_substring", "credential_shaped", "claude_token"):
            total[k] += r[k]
        if any(r[k] for k in ("prefix_substring", "credential_shaped", "claude_token")):
            rows.append(r)
print(json.dumps({"total": total, "files_with_matches": rows}, indent=1))
