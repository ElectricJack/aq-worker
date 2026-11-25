"""Temporary installation hosts and service fixtures shared by the installer checks."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tarfile

import pytest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "packaging/service/aq-install.sh"
CODE = "single-use-enrollment-code"
BINARIES = ("aq-worker", "aq-session-host", "aq-agent", "aq-git")

STUB = """#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
stdin = ""
if name == "aq-worker" and args[:2] == ["service", "enroll"]:
    stdin = sys.stdin.read()
if name in ("sudo", "powershell.exe") or (name == "loginctl" and args[0] == "enable-linger"):
    stdin = sys.stdin.read()
with open(os.environ["CALLS"], "a") as f:
    f.write(json.dumps({"name": name, "args": args, "stdin": stdin}) + "\\n")
if name == "ps":
    print(os.environ.get("STUB_PID1", "systemd"))
if name == "uname":
    print(os.environ.get("STUB_UNAME", "Linux"))
if name == "loginctl":
    linger = pathlib.Path(os.environ["CALLS"] + ".linger")
    if args[0] == "enable-linger":
        if os.environ.get("STUB_REQUIRE_SUDO") and not os.environ.get("STUB_SUDO"):
            sys.exit(1)
        exit_code = int(os.environ.get("STUB_ENABLE_LINGER_RC", "0"))
        if not exit_code and not os.environ.get("STUB_LINGER_UNCHANGED"):
            linger.write_text("yes")
        sys.exit(exit_code)
    print(linger.read_text() if linger.exists() else os.environ.get("STUB_LINGER", "yes"))
if name == "sudo":
    os.execvpe(args[0], args, {**os.environ, "STUB_SUDO": "1"})
if name == "wslpath":
    print(os.environ["WINDOWS_CWD"] if args[0] == "-u" else "C:\\\\AQ install\\\\aq-wsl-autostart.ps1")
if name == "powershell.exe":
    sys.exit(int(os.environ.get("STUB_POWERSHELL_RC", "0")))
if name == "systemctl" and "show-environment" in args:
    sys.exit(int(os.environ.get("STUB_SYSTEMD_USER_RC", "0")))
if name == "systemctl" and "list-units" in args:
    for unit in os.environ.get("STUB_ACTIVE", "").split():
        print(unit + " loaded active running AQ worker service")
if name == "systemctl" and "is-active" in args:
    sys.exit(0 if args[-1] in os.environ.get("STUB_ACTIVE", "").split() else 3)
"""


@pytest.fixture
def host(tmp_path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    stub = bin_dir / "stub"
    stub.write_text(STUB)
    stub.chmod(0o755)
    for name in ("systemctl", "loginctl", "hostname", "ps", "uname", "powershell", "powershell.exe", "wslpath", "sudo"):
        (bin_dir / name).symlink_to(stub)
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    for name in BINARIES:
        shutil.copy(stub, bundle / name)
        (bundle / name).chmod(0o755)
    (bundle / "native.json").write_text("{}")
    (bundle / "repositories.json").write_text("[]")
    home = tmp_path / "home"
    runtime = tmp_path / "run"
    (runtime / "aq-sandbox").mkdir(parents=True)
    home.mkdir()
    sandbox_dir = tmp_path / "sandbox-daemon"
    sandbox_dir.mkdir()
    (sandbox_dir / "aq-sandbox-podman.sh").write_text(
        '#!/usr/bin/env bash\necho "$@" >> "$CALLS_SANDBOX"\n')
    (sandbox_dir / "aq-sandbox-podman.sh").chmod(0o755)
    (sandbox_dir / "aq-sandbox-podman.service").write_text("[Service]\n")
    osrelease = tmp_path / "osrelease"
    osrelease.write_text("6.18.0-generic\n")
    pid1 = tmp_path / "comm"
    pid1.write_text("systemd\n")
    env = {
        "USER": "worker",
        "LANG": "C.UTF-8",
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "HOME": str(home),
        "XDG_RUNTIME_DIR": str(runtime),
        "AQ_OSRELEASE_FILE": str(osrelease),
        "AQ_PROC1_COMM_FILE": str(pid1),
        "AQ_SANDBOX_DAEMON_DIR": str(sandbox_dir),
        "CALLS": str(tmp_path / "calls.jsonl"),
        "CALLS_SANDBOX": str(tmp_path / "sandbox-calls"),
        "WINDOWS_CWD": str(tmp_path),
        "WSL_DISTRO_NAME": "Ubuntu 24.04",
    }
    return {"tmp": tmp_path, "env": env, "bundle": bundle, "runtime": runtime,
            "osrelease": osrelease, "pid1": pid1, "home": home}


def downloadable_bundle(host, monkeypatch):
    spec = importlib.util.spec_from_file_location("prepare_worker_install", SCRIPT.with_name("prepare-worker-install.py"))
    prepare = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(prepare)
    # Registry inspection is unrelated to startup; use a local manifest route.
    monkeypatch.setattr(prepare, "registry_paths", lambda *_: ("127.0.0.1:5000", ["/v2/"]))
    bundle = host["bundle"]
    shutil.copyfile(bundle / "aq-worker", bundle / "tmux")
    refs = {}
    for name, binary in [("helper", "aq-session-host"), ("tmux", "tmux")]:
        path = bundle / binary
        refs[name] = {"path": str(path), "artifact": {"digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()}}
    manifest = bundle / "manifest.json"
    manifest.write_text("{}")
    codex_composite = bundle / "codex-definition.json"
    codex_composite.write_text('{"adapter": "codex"}\n')
    codex_helpers = bundle / "helpers"
    for name in ("codex-code-mode-host", "codex-path/rg", "codex-resources/bwrap"):
        tool = codex_helpers / name
        tool.parent.mkdir(parents=True, exist_ok=True)
        tool.write_text(name)
    native = {
        "adapter_manifest": str(manifest), "inventory_definition": str(manifest),
        "executor": {"composite_definition": str(manifest), "settings": {
            "host": {**refs, "docker_local": {"workspace_bind_paths": {
                "worker_root": "/builder-state", "daemon_root": "/builder-state",
            }, "templates": [{
                "image": "127.0.0.1:5000/aq/sandbox", "digest": "sha256:" + "a" * 64,
            }]}},
            "codex": {"composite_definition": str(codex_composite), "runtime_bundle": str(codex_helpers)},
        }},
    }
    (bundle / "native.json").write_text(json.dumps(native))
    service = host["tmp"] / "service.toml"
    service.write_text("native_template = " + json.dumps(str(bundle / "native.json")) + "\n")
    podman = host["tmp"] / "podman"
    podman.mkdir()
    output = host["tmp"] / "download"
    prepare.build(argparse.Namespace(
        runtime=bundle, worker=bundle / "aq-worker", service=service,
        podman=podman, output=output, codex_helpers=codex_helpers,
        listen="127.0.0.1:28443", worker_port=28081, tls_cert=None, tls_key=None,
    ))
    archive, = (output / "install").glob("*.tar.gz")
    extracted = host["tmp"] / "downloaded"
    with tarfile.open(archive) as tar:
        tar.extractall(extracted, filter="data")
    assert (extracted / "aq-wsl-autostart.ps1").read_bytes() == (ROOT / "packaging/windows/aq-wsl-autostart.ps1").read_bytes()
    return archive, extracted, codex_composite
