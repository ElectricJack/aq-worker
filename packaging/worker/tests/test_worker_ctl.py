"""Fast packaging checks; no real engine, systemd changes, network or sleeps."""
import json
import os
from pathlib import Path
import socket
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[3]
CTL = ROOT / "packaging/worker/aq-worker-ctl"
DRILL = ROOT / "packaging/worker/drill-restart.sh"


@pytest.fixture
def host(tmp_path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    stub = bin_dir / "stub"
    stub.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['CALLS'], 'a') as f:
    f.write(json.dumps([name, *args]) + '\\n')
if name == 'engine' and 'enroll' in args:
    pathlib.Path(os.environ['INPUT']).write_text(sys.stdin.read())
if name == 'systemctl':
    if 'list-units' in args:
        print('aq-worker@loaded.service loaded inactive dead')
    elif 'list-unit-files' in args:
        print('aq-worker@unloaded.service enabled enabled')
    elif 'is-active' in args:
        print('active')
    elif 'status' in args:
        print('active')
if name == 'loginctl':
    print('yes')
""")
    stub.chmod(0o755)
    for name in ("engine", "systemctl", "loginctl"):
        (bin_dir / name).symlink_to(stub)
    wants = tmp_path / "config/systemd/user/default.target.wants"
    wants.mkdir(parents=True)
    # Like systemctl enable, including when the instance is not loaded.
    (wants / "aq-worker@unloaded.service").symlink_to("../aq-worker@.service")
    env = {
        "USER": os.environ.get("USER", "worker"),
        "LANG": "C.UTF-8",
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "HOME": str(tmp_path),
        "XDG_CONFIG_HOME": str(tmp_path / "config"),
        "XDG_STATE_HOME": str(tmp_path / "state"),
        "XDG_RUNTIME_DIR": str(tmp_path / "run"),
        "AQ_WORKER_CONFIG_ROOT": str(tmp_path / "profiles"),
        "AQ_WORKER_STATE_ROOT": str(tmp_path / "journals"),
        "AQ_SANDBOX_SOCKET_DIR": str(tmp_path / "socket"),
        "AQ_WORKER_ENGINE": str(bin_dir / "engine"),
        "AQ_WORKER_IMAGE": "e6a:test",
        "AQ_DRILL_LOG": str(tmp_path / "drill/log"),
        "AQ_DRILL_SCHTASKS": str(tmp_path / "no-windows-interop"),
        "CALLS": str(tmp_path / "calls.jsonl"),
        "INPUT": str(tmp_path / "stdin"),
    }
    return tmp_path, env


def run(host, *args, stdin=None, script=CTL):
    return subprocess.run(
        ["bash", str(script), *args], env=host[1], input=stdin,
        capture_output=True, text=True, timeout=10,
    )


def calls(host):
    return [json.loads(line) for line in Path(host[1]["CALLS"]).read_text().splitlines()]


def enroll_fixture(host, profile):
    result = run(host, "node-config", profile)
    assert result.returncode == 0, result.stderr
    cfg = Path(host[1]["AQ_WORKER_CONFIG_ROOT"]) / profile / "agent-q"
    (cfg / "worker.toml").write_text(f'node_id = "{profile}"\n')
    return cfg


def test_enroll_forwards_secrets_only_on_stdin_and_writes_private_config(host):
    secrets = "enroll-code\nnode-client\nnode-secret\nrunner-client\nrunner-secret\n"
    result = run(host, "enroll", "home", "--server", "https://machine.example",
                 "--cf-service-token", "--runner-access-token", stdin=secrets)
    assert result.returncode == 0, result.stderr
    assert Path(host[1]["INPUT"]).read_text() == secrets
    assert not any(secret in json.dumps(calls(host)) + result.stdout + result.stderr
                   for secret in secrets.splitlines())
    cfg = Path(host[1]["AQ_WORKER_CONFIG_ROOT"]) / "home/agent-q/node.json"
    node = json.loads(cfg.read_text())
    assert node["docker_local"]["socket"] == "/run/aq-sandbox/podman.sock"
    assert node["state_dir"] == "/state/controller"
    assert cfg.stat().st_mode & 0o777 == 0o600
    assert cfg.parent.stat().st_mode & 0o777 == 0o700


def test_container_mounts_only_profile_state_and_dedicated_socket(host):
    enroll_fixture(host, "home")
    socket_dir = Path(host[1]["AQ_SANDBOX_SOCKET_DIR"])
    socket_dir.mkdir()
    with socket.socket(socket.AF_UNIX) as daemon:
        daemon.bind(str(socket_dir / "podman.sock"))
        result = run(host, "container-run", "home")
    assert result.returncode == 0, result.stderr
    argv = next(c[1:] for c in calls(host) if c[:2] == ["engine", "run"])
    assert "--init" in argv
    assert argv[argv.index("--user") + 1] == f"{os.getuid()}:{os.getgid()}"
    assert argv[argv.index("--cap-drop") + 1] == "ALL"
    mounts = [argv[i + 1] for i, arg in enumerate(argv) if arg == "-v"]
    assert mounts == [f"{host[1]['AQ_WORKER_CONFIG_ROOT']}/home:/config",
                      f"{host[1]['AQ_WORKER_STATE_ROOT']}/home:/state",
                      f"{socket_dir}:/run/aq-sandbox"]
    assert "docker.sock" not in json.dumps(argv)
    assert argv[-3:] == ["run", "--controller-config", "/config/agent-q/node.json"]


def test_switch_disables_enabled_unloaded_profiles_and_preserves_state(host):
    for profile in ("home", "loaded", "unloaded"):
        enroll_fixture(host, profile)
        (Path(host[1]["AQ_WORKER_STATE_ROOT"]) / profile / "journal.json").write_text(profile)
    files = {p: p.read_bytes() for key in ("AQ_WORKER_CONFIG_ROOT", "AQ_WORKER_STATE_ROOT")
             for p in Path(host[1][key]).rglob("*") if p.is_file()}
    result = run(host, "use", "home")
    assert result.returncode == 0, result.stderr
    commands = calls(host)
    for old in ("loaded", "unloaded"):
        assert ["systemctl", "--user", "disable", "--now", f"aq-worker@{old}.service"] in commands
    assert ["systemctl", "--user", "disable", "--now", "aq-worker@.service"] not in commands
    assert ["systemctl", "--user", "enable", "--now", "aq-worker@home.service"] in commands
    assert all(p.read_bytes() == body for p, body in files.items())


def test_uninstall_disables_enabled_unloaded_profiles_but_keeps_data(host):
    cfg = enroll_fixture(host, "unloaded")
    journal = Path(host[1]["AQ_WORKER_STATE_ROOT"]) / "unloaded/journal.json"
    journal.write_text("retained journal")
    result = run(host, "uninstall")
    assert result.returncode == 0, result.stderr
    assert ["systemctl", "--user", "disable", "--now", "aq-worker@unloaded.service"] in calls(host)
    assert (cfg / "worker.toml").exists()
    assert journal.read_text() == "retained journal"


@pytest.mark.parametrize("new_boot, expected", [(True, 0), (False, 1)])
def test_restart_drill_requires_new_boot_with_same_node_and_online_server(host, new_boot, expected):
    journal = Path(host[1]["XDG_STATE_HOME"]) / "agent-q/workers/home/controller/journal.json"
    journal.parent.mkdir(parents=True)
    state = {"identity": {"node_id": "node-1", "boot_id": "boot-1"}, "revision": 1}
    journal.write_text(json.dumps({"state": state}))
    api = host[0] / "workers.json"
    api.write_text(json.dumps({"workers": [{"id": "node-1", "status": "online"}]}))
    before = run(host, "before", "home", api.as_uri(), script=DRILL)
    assert before.returncode == 0, before.stderr
    state["revision"] += 1
    if new_boot:
        state["identity"]["boot_id"] = "boot-2"
    journal.write_text(json.dumps({"state": state}))
    after = run(host, "after", "home", api.as_uri(), script=DRILL)
    assert after.returncode == expected, after.stderr
    assert ("DRILL PASS" if new_boot else "FAIL new boot_id") in after.stdout


def test_machine_config_validates_key_before_writing(host):
    bad = run(host, "machine-config", "local", "Bad_Key")
    assert bad.returncode != 0
    assert "invalid" in bad.stderr
    path = Path(host[1]["AQ_WORKER_CONFIG_ROOT"]) / "local" / "agent-q" / "machine.json"
    assert not path.exists()
    good = run(host, "machine-config", "local", "desk-1")
    assert good.returncode == 0, good.stderr
    assert json.loads(path.read_text())["key"] == "desk-1"
