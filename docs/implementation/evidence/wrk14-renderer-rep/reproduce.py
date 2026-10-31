#!/usr/bin/env python3
"""Finite tmux screen oracle only; no provider or host/stopped-proof claim."""
import json
import pathlib
import subprocess
import tempfile
import time

cases = [
    ("1b5b324a1b5b48411b5b33627c", "AAAA|"),
    ("1b5b324a1b5b4865cc811b5b33627c", "e\u0301|"),
]
observed = []
for data, expected in cases:
    with tempfile.TemporaryDirectory(prefix="aq-rep-") as root:
        sock = str(pathlib.Path(root) / "tmux.sock")
        server = subprocess.Popen(["/usr/bin/tmux", "-D", "-u", "-S", sock, "-f", "/dev/null"],
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def tmux(*args):
            return subprocess.run(["/usr/bin/tmux", "-N", "-S", sock, *args], check=True,
                                  capture_output=True, timeout=1).stdout
        try:
            end = time.monotonic() + 2
            while not pathlib.Path(sock).exists():
                if server.poll() is not None or time.monotonic() >= end:
                    raise RuntimeError("private synthetic oracle server unavailable")
                time.sleep(.01)
            tmux("set-option", "-g", "status", "off")
            code = "import os,time;os.write(1,bytes.fromhex('" + data + "'));time.sleep(3)"
            tmux("new-session", "-d", "-s", "aq", "-x", "20", "-y", "4", "--", "/usr/bin/python3", "-c", code)
            end = time.monotonic() + 2
            while True:
                capture = tmux("capture-pane", "-p", "-t", "aq:0.0").decode("utf-8")
                if "|" in capture:
                    break
                if time.monotonic() >= end:
                    raise RuntimeError("synthetic trailing marker unavailable")
                time.sleep(.01)
            assert capture.splitlines()[0] == expected, repr(capture)
            observed.append({"sequence_utf8_hex": data, "columns": 20, "pane_rows": 4,
                             "status": "off", "capture": capture, "tail_marker_observed": True})
        finally:
            try:
                tmux("kill-server")
            except (subprocess.SubprocessError, OSError):
                pass
            try:
                server.wait(timeout=1)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=1)
print(json.dumps({"tmux_version": subprocess.check_output(["/usr/bin/tmux", "-V"], text=True).strip(),
                  "scope": "screen oracle only; not guarded-host or renderer compatibility qualification",
                  "cases": observed}, indent=2))
