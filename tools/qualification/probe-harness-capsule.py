#!/usr/bin/env python3
"""Opt-in kernel-only capsule canary; no bwrap, agent, provider or runtime gate.
Requires Linux unprivileged namespaces, close_range, pidfd, cc and static libc.
Compiler/Python are qualification tools, not aq-worker runtime dependencies.
"""
import pathlib
import secrets
import subprocess
import tempfile

source = pathlib.Path(__file__).with_name("harness-capsule-peer.c")
with tempfile.TemporaryDirectory(prefix="aq-capsule-canary-") as temporary:
    root = pathlib.Path(temporary)
    for name in ("root", "private-node", "workspace", "auth", "allowed", "git-metadata"):
        (root / name).mkdir(mode=0o700)
    original = secrets.token_bytes(32)
    (root / "private-node/material").write_bytes(original)
    (root / "auth/auth.json").write_bytes(b"derived-capability-canary\n")
    (root / "allowed/item").write_bytes(b"allowed-folder-canary\n")
    (root / "git-metadata/item").write_bytes(b"internal-git-canary\n")
    executable = root / "peer"
    subprocess.run(["/usr/bin/cc", "-static", "-O2", "-Wall", "-Wextra", "-pthread", str(source), "-o", str(executable)], check=True, env={"PATH": "/usr/bin:/bin"})
    result = subprocess.run([str(executable), str(root)], env={"PATH": "/usr/bin:/bin"}, timeout=15, capture_output=True, text=True)
    print(result.stdout, end="")
    print(result.stderr, end="")
    if result.returncode:
        raise SystemExit(result.returncode)
    assert (root / "workspace/result").is_file()
    assert (root / "private-node/material").read_bytes() == original
    print("PASS parent original material unchanged; no external connections or agent executable used")
