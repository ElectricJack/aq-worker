"""Install script checks: fake PATH stubs and a temporary HOME; nothing real is installed."""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "packaging/service/aq-install.sh"
CODE = "single-use-enrollment-code"
BINARIES = ("aq-worker", "aq-session-host", "aq-agent", "aq-git", "aq-sandbox-driver")

STUB = """#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
stdin = ""
if name == "aq-worker" and args[:2] == ["service", "enroll"]:
    stdin = sys.stdin.read()
with open(os.environ["CALLS"], "a") as f:
    f.write(json.dumps({"name": name, "args": args, "stdin": stdin}) + "\\n")
if name == "ps":
    print(os.environ.get("STUB_PID1", "systemd"))
if name == "uname":
    print(os.environ.get("STUB_UNAME", "Linux"))
if name == "loginctl":
    print(os.environ.get("STUB_LINGER", "yes"))
if name == "systemctl" and "show-environment" in args:
    sys.exit(int(os.environ.get("STUB_SYSTEMD_USER_RC", "0")))
"""


@pytest.fixture
def host(tmp_path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    stub = bin_dir / "stub"
    stub.write_text(STUB)
    stub.chmod(0o755)
    for name in ("systemctl", "loginctl", "hostname", "ps", "uname", "powershell", "powershell.exe"):
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
    env = {
        "USER": "worker",
        "LANG": "C.UTF-8",
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "HOME": str(home),
        "XDG_RUNTIME_DIR": str(runtime),
        "AQ_OSRELEASE_FILE": str(osrelease),
        "AQ_SANDBOX_DAEMON_DIR": str(sandbox_dir),
        "CALLS": str(tmp_path / "calls.jsonl"),
        "CALLS_SANDBOX": str(tmp_path / "sandbox-calls"),
    }
    return {"tmp": tmp_path, "env": env, "bundle": bundle, "runtime": runtime,
            "osrelease": osrelease}


def with_socket(host):
    path = host["runtime"] / "aq-sandbox/podman.sock"
    daemon = socket.socket(socket.AF_UNIX)
    daemon.bind(str(path))
    return daemon


def run(host, *args, stdin=CODE + "\n", env=None):
    full = {**host["env"], **(env or {})}
    return subprocess.run(
        ["bash", str(SCRIPT), "--server", "https://machine.example",
         "--bundle", str(host["bundle"]), *args],
        env=full, input=stdin, capture_output=True, text=True, timeout=20)


def calls(host):
    path = Path(host["env"]["CALLS"])
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def worker_calls(host):
    return [c for c in calls(host) if c["name"] == "aq-worker"]


def test_shellcheck_when_present():
    if shutil.which("shellcheck") is None:
        pytest.skip("shellcheck is not installed")
    result = subprocess.run(["shellcheck", str(SCRIPT)], capture_output=True, text=True)
    assert result.returncode == 0, result.stdout


def test_refuses_wsl_without_systemd(host):
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    result = run(host, env={"STUB_PID1": "init"})
    assert result.returncode == 3
    assert "systemd=true" in result.stderr and "/etc/wsl.conf" in result.stderr
    assert worker_calls(host) == []


def test_refuses_macos_with_a_clear_message(host):
    result = run(host, env={"STUB_UNAME": "Darwin"})
    assert result.returncode == 2
    assert "Only Linux and WSL2 are supported." in result.stderr


def test_never_passes_secrets_in_argv(host):
    with with_socket(host):
        result = run(host, "--cf-service-token")
    assert result.returncode == 0, result.stderr
    enroll = next(c for c in worker_calls(host) if c["args"][:2] == ["service", "enroll"])
    assert enroll["stdin"] == CODE + "\n"
    assert "--cf-service-token" in enroll["args"]
    assert CODE not in json.dumps(calls(host)).replace(enroll["stdin"].strip(), "")
    assert CODE not in result.stdout + result.stderr


def test_installs_binaries_and_calls_enroll_install_measure_in_order(host):
    token = host["tmp"] / "token"
    token.write_text("x")
    with with_socket(host):
        result = run(host, "--profile", "p1", "--slots", "3", "--name", "box", "--machine-key", "box-1",
                     "--measurement-token-file", str(token), "--measurement-model", "claude-sonnet-5-5")
    assert result.returncode == 0, result.stderr
    runtime = Path(host["env"]["HOME"]) / ".local/share/agent-q/runtime"
    for name in BINARIES:
        assert (runtime / name).stat().st_mode & 0o777 == 0o755
    link = Path(host["env"]["HOME"]) / ".local/bin/aq-worker"
    assert link.is_symlink() and link.resolve() == (runtime / "aq-worker").resolve()
    steps = [c["args"][:2] for c in worker_calls(host)]
    assert steps == [["service", "enroll"], ["service", "install"], ["service", "measure"],
                     ["service", "status"]]
    enroll = worker_calls(host)[0]["args"]
    assert enroll[enroll.index("--slots") + 1] == "3"
    assert enroll[enroll.index("--name") + 1] == "box"
    assert enroll[enroll.index("--machine-key") + 1] == "box-1"
    assert enroll[enroll.index("--aq-git") + 1] == str(runtime / "aq-git")
    assert enroll[enroll.index("--measurement-token-file") + 1] == str(token)
    assert "allow_enrolled_nodes" in result.stdout


def test_defaults_template_and_repositories_from_the_bundle(host):
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    enroll = worker_calls(host)[0]["args"]
    assert enroll[enroll.index("--native-template") + 1] == str(host["bundle"] / "native.json")
    assert enroll[enroll.index("--repositories") + 1] == str(host["bundle"] / "repositories.json")
    assert enroll[enroll.index("--profile") + 1] == "local"
    assert enroll[enroll.index("--slots") + 1] == "1"
    assert "--measurement-token-file" not in enroll


@pytest.mark.parametrize("key", ["Bad", "-x", "a_b", "a" * 64, ""])
def test_refuses_an_invalid_machine_key(host, key):
    with with_socket(host):
        result = run(host, "--machine-key", key)
    assert result.returncode == 2
    assert worker_calls(host) == []


def test_prints_but_never_runs_the_windows_probe(host):
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    assert "aq-host-load.ps1 -Install -StartNow" in result.stdout
    assert not [c for c in calls(host) if c["name"].startswith("powershell")]


def test_exits_4_without_sandbox_socket_or_bundle(host):
    result = run(host)
    assert result.returncode == 4
    assert "--sandbox-bundle" in result.stderr
    assert worker_calls(host) == []


def test_sandbox_bundle_installs_and_enables_the_daemon(host):
    tarball = host["tmp"] / "podman-linux-amd64.tar.gz"
    tarball.write_text("x")
    result = run(host, "--sandbox-bundle", str(tarball))
    assert result.returncode == 0, result.stderr
    assert Path(host["env"]["CALLS_SANDBOX"]).read_text().strip() == f"install {tarball}"
    systemctl = [c["args"] for c in calls(host) if c["name"] == "systemctl"]
    assert ["--user", "enable", "--now", "aq-sandbox-podman.service"] in systemctl
