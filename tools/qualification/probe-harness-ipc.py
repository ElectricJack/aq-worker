#!/usr/bin/env python3
"""Kernel-only IPC-policy probe, complementary to the capsule view canary.
No harness/provider or runtime gate. Requires rootless user/net/IPC namespaces,
x86_64 seccomp, close_range, cc and static libc. No bwrap dependency.
"""
import pathlib
import subprocess
import tempfile

source = pathlib.Path(__file__).with_name("harness-ipc-peer.c")
with tempfile.TemporaryDirectory(prefix="aq-ipc-") as temporary:
    root = pathlib.Path(temporary)
    executable = root / "peer"
    subprocess.run(["/usr/bin/cc", "-static", "-O2", "-Wall", "-Wextra", str(source), "-o", str(executable)], check=True, env={"PATH": "/usr/bin:/bin"})
    result = subprocess.run([str(executable), str(root)], env={"PATH": "/usr/bin:/bin"}, timeout=15, capture_output=True, text=True)
    print(result.stdout, end="")
    print(result.stderr, end="")
    raise SystemExit(result.returncode)
