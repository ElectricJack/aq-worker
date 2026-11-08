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
    }
    return {"tmp": tmp_path, "env": env, "bundle": bundle, "runtime": runtime,
            "osrelease": osrelease, "pid1": pid1, "home": home}


def with_socket(host):
    path = host["runtime"] / "aq-sandbox/podman.sock"
    path.unlink(missing_ok=True)
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
    host["pid1"].write_text("init\n")
    result = run(host)
    assert result.returncode == 3
    assert "systemd=true" in result.stderr and "/etc/wsl.conf" in result.stderr
    assert worker_calls(host) == []
    # PID 1 comes from /proc/1/comm, not from ps.
    assert not [c for c in calls(host) if c["name"] == "ps"]
    host["pid1"].write_text("systemd\n")
    with with_socket(host):
        assert run(host).returncode == 0


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
    # The code reaches only the enroll call's stdin: no argv, and no other call's stdin.
    for c in calls(host):
        assert CODE not in json.dumps(c["args"]), c
        assert c == enroll or CODE not in c["stdin"], c
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


@pytest.mark.parametrize("profile", ["Bad", "a_b", "a.b", "-x", "a" * 33])
def test_profile_follows_the_service_config_rule(host, profile):
    with with_socket(host):
        result = run(host, "--profile", profile)
    assert result.returncode == 2
    assert worker_calls(host) == []


def test_refuses_to_replace_a_foreign_aq_worker(host):
    link = host["home"] / ".local/bin/aq-worker"
    link.parent.mkdir(parents=True)
    link.write_text("#!/bin/sh\n")
    with with_socket(host):
        result = run(host)
    assert result.returncode == 2 and "not the link" in result.stderr
    assert link.read_text() == "#!/bin/sh\n" and worker_calls(host) == []
    link.unlink()
    link.symlink_to(host["tmp"] / "elsewhere")
    with with_socket(host):
        assert run(host).returncode == 2
    link.unlink()
    with with_socket(host):
        assert run(host).returncode == 0
        assert run(host, "--reconfigure").returncode == 0, "our own link is replaced"


def test_warns_when_the_container_worker_is_active(host):
    with with_socket(host):
        quiet = run(host)
        loud = run(host, env={"STUB_ACTIVE": "aq-worker@local.service"})
    assert quiet.returncode == 0 and "aq-worker@local" not in quiet.stderr
    assert loud.returncode == 0 and "aq-worker@local is running" in loud.stderr


def test_an_enrolled_profile_is_not_enrolled_again_without_reconfigure(host):
    profile = host["home"] / ".config/agent-q/service/local"
    profile.mkdir(parents=True)
    (profile / "service.toml").write_text("")
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    assert "already enrolled" in result.stdout
    steps = [c["args"][:2] for c in worker_calls(host)]
    assert steps == [["service", "install"], ["service", "status"]]
    with with_socket(host):
        assert run(host, "--reconfigure").returncode == 0
    enroll = [c["args"] for c in worker_calls(host) if c["args"][:2] == ["service", "enroll"]]
    assert len(enroll) == 1 and "--reconfigure" in enroll[0]
    (profile / "enroll-pending.toml").write_text("")
    with with_socket(host):
        assert run(host).returncode == 0
    assert len([c for c in worker_calls(host) if c["args"][:2] == ["service", "enroll"]]) == 2, \
        "an unfinished enrollment resumes"
