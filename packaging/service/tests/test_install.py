"""Install script checks: fake PATH stubs and a temporary HOME; nothing real is installed."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tarfile
import tomllib

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


def with_socket(host):
    path = host["runtime"] / "aq-sandbox/podman.sock"
    path.unlink(missing_ok=True)
    daemon = socket.socket(socket.AF_UNIX)
    daemon.bind(str(path))
    return daemon


def run(host, *args, stdin=CODE + "\n", env=None, script=SCRIPT):
    full = {**host["env"], **(env or {})}
    return subprocess.run(
        ["bash", str(script), "--server", "https://machine.example",
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
    powershell, = [c for c in calls(host) if c["name"] == "powershell.exe"]
    assert "aq-host-load.ps1" not in " ".join(powershell["args"])
    assert powershell["args"] == [
        "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
        "-File", "C:\\AQ install\\aq-wsl-autostart.ps1", "-Distro", "Ubuntu 24.04",
        "-StartNow", "-NoDockerDesktop",
    ]


@pytest.mark.parametrize("require_sudo", [False, True])
def test_enables_linger_without_consuming_enrollment_stdin(host, require_sudo):
    with with_socket(host):
        result = run(host, env={"STUB_LINGER": "no", **({"STUB_REQUIRE_SUDO": "1"} if require_sudo else {})})
    assert result.returncode == 0, result.stderr
    log = calls(host)
    enabled = [c for c in log if c["name"] == "loginctl" and c["args"] == ["enable-linger", "worker"]]
    assert len(enabled) == (2 if require_sudo else 1)
    assert bool([c for c in log if c["name"] == "sudo"]) == require_sudo
    assert log.index(enabled[-1]) < log.index(worker_calls(host)[0])
    assert worker_calls(host)[0]["stdin"] == CODE + "\n"


def test_existing_linger_is_not_changed_and_linux_needs_no_windows_tools(host):
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    assert not [c for c in calls(host) if c["name"] in ("sudo", "powershell.exe", "wslpath")]
    assert not [c for c in calls(host) if c["name"] == "loginctl" and "enable-linger" in c["args"]]
    assert "starts at boot" in result.stdout


@pytest.mark.parametrize("failure", [{"STUB_ENABLE_LINGER_RC": "1"}, {"STUB_LINGER_UNCHANGED": "1"}])
def test_refuses_to_enroll_when_boot_startup_cannot_be_enabled(host, failure):
    with with_socket(host):
        result = run(host, env={"STUB_LINGER": "no", **failure})
    assert result.returncode == 3
    assert "boot startup" in result.stderr
    assert worker_calls(host) == []


def test_windows_startup_failure_does_not_consume_an_enrollment_code(host):
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    with with_socket(host):
        result = run(host, env={"STUB_POWERSHELL_RC": "1"})
    assert result.returncode == 3
    assert "Windows autostart could not be configured" in result.stderr
    assert worker_calls(host) == []


def test_wsl_requires_a_distro_name_before_enrollment(host):
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    with with_socket(host):
        result = run(host, env={"WSL_DISTRO_NAME": ""})
    assert result.returncode == 3
    assert "WSL_DISTRO_NAME" in result.stderr
    assert worker_calls(host) == []


def test_wsl_requires_the_windows_script_in_a_standalone_installer(host):
    script = host["bundle"] / "aq-install.sh"
    shutil.copyfile(SCRIPT, script)
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    with with_socket(host):
        result = run(host, script=script)
    assert result.returncode == 3
    assert "bundled aq-wsl-autostart.ps1" in result.stderr
    assert worker_calls(host) == []


def test_existing_sandbox_socket_also_gets_an_enabled_service(host):
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    assert ["--user", "enable", "--now", "aq-sandbox-podman.service"] in [
        c["args"] for c in calls(host) if c["name"] == "systemctl"
    ]


def test_downloadable_bundle_installs_windows_startup_without_a_checkout(host, monkeypatch):
    spec = importlib.util.spec_from_file_location("prepare_worker_install", SCRIPT.with_name("prepare-worker-install.py"))
    prepare = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(prepare)
    # Registry inspection is unrelated to startup; use a local manifest route.
    monkeypatch.setattr(prepare, "registry_paths", lambda *_: ("127.0.0.1:5000", ["/v2/"]))
    bundle = host["bundle"]
    shutil.copyfile(bundle / "aq-worker", bundle / "tmux")
    refs = {}
    for name, binary in [("helper", "aq-session-host"), ("driver", "aq-sandbox-driver"), ("tmux", "tmux")]:
        path = bundle / binary
        refs[name] = {"path": str(path), "artifact": {"digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()}}
    manifest = bundle / "manifest.json"
    manifest.write_text("{}")
    native = {
        "adapter_manifest": str(manifest), "inventory_definition": str(manifest),
        "executor": {"composite_definition": str(manifest), "settings": {"host": {
            **refs, "docker_local": {"templates": [{"image": "test", "digest": "sha256:" + "a" * 64}]},
        }}},
    }
    (bundle / "native.json").write_text(json.dumps(native))
    service = host["tmp"] / "service.toml"
    service.write_text("native_template = " + json.dumps(str(bundle / "native.json")) + "\n")
    podman = host["tmp"] / "podman"
    podman.mkdir()
    output = host["tmp"] / "download"
    prepare.build(argparse.Namespace(
        runtime=bundle, worker=bundle / "aq-worker", service=service,
        podman=podman, output=output, codex_helpers=None,
        listen="127.0.0.1:28443", worker_port=28081, tls_cert=None, tls_key=None,
    ))
    archive, = (output / "install").glob("*.tar.gz")
    extracted = host["tmp"] / "downloaded"
    with tarfile.open(archive) as tar:
        tar.extractall(extracted, filter="data")
    assert (extracted / "aq-wsl-autostart.ps1").read_bytes() == (ROOT / "packaging/windows/aq-wsl-autostart.ps1").read_bytes()
    host["bundle"] = extracted
    host["osrelease"].write_text("5.15.0-microsoft-standard-WSL2\n")
    with with_socket(host):
        result = run(host, script=extracted / "aq-install.sh")
    assert result.returncode == 0, result.stderr
    assert worker_calls(host)[0]["stdin"] == CODE + "\n"
    assert any(c["name"] == "powershell.exe" for c in calls(host))
    assert {"name": "wslpath", "args": ["-w", str(extracted / "aq-wsl-autostart.ps1")], "stdin": ""} in calls(host)


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


def test_deferred_install_enrolls_without_starting_a_service(host):
    with with_socket(host):
        result = run(host, "--defer-start")
    assert result.returncode == 0, result.stderr
    assert [call["args"][:2] for call in worker_calls(host)] == [["service", "enroll"]]
    assert worker_calls(host)[0]["stdin"] == CODE + "\n"


@pytest.mark.parametrize("manual_sessions", [False, True])
@pytest.mark.parametrize("with_token", [False, True])
def test_hosted_install_keeps_the_bundled_worker_config_schema(tmp_path, manual_sessions, with_token):
    (tmp_path / "bootstrap.json").write_text(json.dumps({
        "manual_sessions": manual_sessions, "source_slugs": {},
    }))
    service = tmp_path / "service.toml"
    contents = "slots = 1\n"
    if with_token:
        contents += '\n[measurement]\ntoken_file = "/private/claude-token"\n'
    service.write_text(contents)
    result = subprocess.run(
        [sys.executable, str(SCRIPT.with_name("worker-install-config.py")),
         "configure", str(tmp_path), str(tmp_path), "https://worker.example",
         "/usr/bin/podman", "1", "a" * 64],
        capture_output=True, text=True, timeout=20,
    )
    assert result.returncode == 0, result.stderr
    assert ("manual" in tomllib.loads(service.read_text())) == (manual_sessions and with_token)


def test_hosted_install_repairs_the_tokenless_manual_section_and_keeps_enrollment(tmp_path):
    (tmp_path / "bootstrap.json").write_text(json.dumps({
        "manual_sessions": True, "source_slugs": {},
    }))
    service = tmp_path / "service.toml"
    original = 'slots = 1\nmachine_key = "existing-pc"\n\n[measurement]\nenabled = false\n'
    service.write_text(original.rstrip() + '\n\n[manual]\npodman_executable = "/usr/bin/podman"\nmax_slots = 1\n\n[manual.source_slugs]\n')
    enrollment = tmp_path / "worker.toml"
    enrollment.write_text('credential = "existing-enrollment"\n')
    for _ in range(2):
        result = subprocess.run(
            [sys.executable, str(SCRIPT.with_name("worker-install-config.py")),
             "configure", str(tmp_path), str(tmp_path), "https://worker.example",
             "/usr/bin/podman", "1", "a" * 64],
            capture_output=True, text=True, timeout=20,
        )
        assert result.returncode == 0, result.stderr
        assert service.read_text() == original
        assert enrollment.read_text() == 'credential = "existing-enrollment"\n'


def login_helper():
    import importlib.util
    spec = importlib.util.spec_from_file_location("aq_login_config", SCRIPT.parent / "worker-install-config.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def login_profile(tmp_path):
    package, config = tmp_path / "package", tmp_path / "config"
    package.mkdir(); config.mkdir()
    (package / "codex-bundle").mkdir()
    (package / "bootstrap.json").write_text(json.dumps({"source_slugs": {"repo": "owner/repo"}}))
    (config / "login-sync.json").write_text(json.dumps({"podman": "/opt/podman"}))
    (config / "service.toml").write_text('profile = "local"\nslots = 1\n[measurement]\nenabled = false\nmodel = "fixture-model"\n')
    return package, config


def test_server_logins_enable_manual_both_providers_and_remain_private(tmp_path):
    helper = login_helper()
    package, config = login_profile(tmp_path)
    codex = json.dumps({"auth_mode": "chatgpt", "tokens": {"access_token": "test-access", "refresh_token": "test-refresh", "id_token": "test-id", "account_id": "test-account"}})
    logins = [{"provider": "anthropic", "material": "synthetic-claude"}, {"provider": "openai", "material": codex}]
    assert helper.apply_logins(package, config, logins)
    parsed = helper.read_toml(config / "service.toml")
    assert parsed["manual"]["source_slugs"] == {"repo": "owner/repo"}
    assert parsed["measurement"]["enabled"] is False
    assert parsed["measurement"]["model"] == "fixture-model"
    assert parsed["measurement"]["token_file"] == str(config / "provider-auth/claude-token")
    assert parsed["manual"]["codex"]["auth_file"] == str(config / "provider-auth/codex-auth.json")
    for path in (config / "provider-auth").iterdir():
        assert path.stat().st_mode & 0o777 == 0o600
    assert (config / "provider-auth").stat().st_mode & 0o777 == 0o700
    assert not helper.apply_logins(package, config, logins)


def test_codex_only_then_claude_requests_capability_restart(tmp_path):
    helper = login_helper(); package, config = login_profile(tmp_path)
    auth = json.dumps({"auth_mode": "chatgpt", "tokens": {"access_token": "a", "refresh_token": "r", "id_token": "i", "account_id": "acct"}})
    assert helper.apply_logins(package, config, [{"provider": "openai", "material": auth}])
    assert not (config / "provider-auth/claude-token").exists()
    assert helper.apply_logins(package, config, [{"provider": "anthropic", "material": "synthetic"}])


def test_invalid_delivery_writes_no_secrets(tmp_path):
    helper = login_helper(); package, config = login_profile(tmp_path)
    for logins in ([], [{"provider": "anthropic", "material": "bad\nvalue"}], [{"provider": "openai", "material": '{"OPENAI_API_KEY":"test"}'}]):
        with pytest.raises(ValueError):
            helper.apply_logins(package, config, logins)
        assert not (config / "provider-auth").exists()


def test_receipt_retry_does_not_restart_twice(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    import uuid
    generation = str(uuid.uuid4()); requests = []; commands = []
    def request(_config, body, _slot=1):
        requests.append(body)
        if not body:
            return {"generation": generation, "logins": [{"provider": "anthropic", "material": "synthetic"}]}
        if len(requests) == 2:
            raise OSError("simulated lost acknowledgement")
        return {"generation": None, "logins": []}
    monkeypatch.setattr(helper, "login_request", request)
    monkeypatch.setattr(helper, "sync_diagnostics_slot", lambda *_args: None)
    monkeypatch.setattr(helper, "ensure_terminal_relay", lambda *_args: None)
    monkeypatch.setattr(helper.subprocess, "run", lambda argv, **_kw: commands.append(argv))
    with pytest.raises(ValueError):
        helper.sync_logins(package, config)
    helper.sync_logins(package, config)
    assert len(commands) == 1
    assert requests[1] == requests[3] == {"generation": generation, "applied": True}


def test_manual_terminal_companion_is_installed_and_kept_alive_without_restarting_worker(tmp_path, monkeypatch):
    from types import SimpleNamespace
    helper = login_helper(); package, config = login_profile(tmp_path)
    worker = package / 'aq-worker'; worker.write_text('fixture'); worker.chmod(0o755)
    original = (config / 'service.toml').read_text()
    original = 'state_root = ' + json.dumps(str(config / 'state')) + '\n' + original + '\n[manual]\nmax_slots = 2\n'
    (config / 'service.toml').write_text(original)
    monkeypatch.setenv('XDG_CONFIG_HOME', str(tmp_path / 'xdg'))
    commands = []
    active = False
    def run(args, **_kwargs):
        commands.append(args)
        return SimpleNamespace(returncode=0 if active or 'is-active' not in args else 3)
    monkeypatch.setattr(helper.subprocess, 'run', run)
    helper.ensure_terminal_relay(package, config)
    unit = (tmp_path / 'xdg/systemd/user/aq-terminal-relay-local.service').read_text()
    assert '"' + str(worker) + '" "terminal-relay" "--service-config" "' + str(config / 'service.toml') + '"' in unit
    assert 'Restart=always' in unit and 'UMask=0077' in unit
    assert (config / 'state/manual-agents').is_dir()
    assert commands[-1] == ['systemctl', '--user', 'restart', 'aq-terminal-relay-local.service']
    assert not any('aq-node@local.service' in args for args in commands)
    assert (config / 'service.toml').read_text() == original
    active = True; commands.clear()
    helper.ensure_terminal_relay(package, config)
    assert commands == [['systemctl', '--user', 'is-active', '--quiet', 'aq-terminal-relay-local.service']]
    active = False; commands.clear()
    helper.ensure_terminal_relay(package, config)
    assert commands[-1] == ['systemctl', '--user', 'start', 'aq-terminal-relay-local.service']


def test_unconfigured_provider_does_not_start_terminal_companion(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    monkeypatch.setattr(helper.subprocess, 'run', lambda *_args, **_kwargs: pytest.fail('No manual configuration'))
    helper.ensure_terminal_relay(package, config)


def test_connected_receiver_repairs_missing_terminal_companion_even_without_a_new_login(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    seen = []
    monkeypatch.setattr(helper, 'sync_slot', lambda *_args: None)
    monkeypatch.setattr(helper, 'sync_diagnostics_slot', lambda *_args: None)
    monkeypatch.setattr(helper, 'ensure_terminal_relay', lambda p, c: seen.append((p, c)))
    helper.sync_logins(package, config)
    assert seen == [(package, config)]


def test_login_unit_uses_profile_and_persistent_helper(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))
    commands = []
    monkeypatch.setattr(helper.subprocess, "run", lambda argv, **_kw: commands.append(argv))
    helper.install_sync(package, config)
    unit = (tmp_path / "xdg/systemd/user/aq-login-sync-local.service").read_text()
    assert str(config / "login-sync.py") in unit
    assert "UMask=0077" in unit
    assert commands[-1][-1] == "aq-login-sync-local.timer"


def test_retired_primary_does_not_prevent_sibling_delivery(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'service.toml').write_text('profile = "local"\nslots = 2\n')
    seen = []
    def sync(_package, _config, slot):
        seen.append(slot)
        if slot == 1:
            raise OSError('revoked primary')
    monkeypatch.setattr(helper, 'sync_slot', sync)
    monkeypatch.setattr(helper, 'sync_diagnostics_slot', lambda *_args: None)
    with pytest.raises(ValueError):
        helper.sync_logins(package, config)
    assert seen == [1, 2]


def preflight_profile(tmp_path, monkeypatch, info=None, failure=None):
    from types import SimpleNamespace
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'native.json').write_text(json.dumps({'executor': {'settings': {'host': {'docker_local': {
        'socket': '/run/aq-test/podman.sock', 'templates': [{'image': 'registry.example/template@sha256:fixture'}],
    }}}}}))
    data = info if info is not None else {'SecurityOptions': ['name=rootless'], 'CgroupVersion': '2', 'CgroupDriver': 'systemd'}
    class Connection:
        def __init__(self, *_args, **_kwargs):
            pass
        def request(self, *args):
            assert args == ('GET', '/v1.44/info')
        def getresponse(self):
            return SimpleNamespace(status=200, read=lambda _size: json.dumps(data).encode())
        def close(self):
            pass
    commands = []
    monkeypatch.setattr(helper.http.client, 'HTTPConnection', Connection)
    def execute(args, **_kwargs):
        commands.append(args)
        return SimpleNamespace(returncode=1 if failure and 'run' in args else 0, stderr=failure or '')
    monkeypatch.setattr(helper.subprocess, 'run', execute)
    return helper, package, config, commands


def test_preflight_exercises_real_launch_limits_bridge_and_tmux(tmp_path, monkeypatch):
    helper, package, config, commands = preflight_profile(tmp_path, monkeypatch)
    helper.sandbox_preflight(package, config, Path('/opt/podman'))
    launch, cleanup = commands
    assert launch[launch.index('--network') + 1] == 'bridge'
    assert launch[launch.index('--memory') + 1] == '4g'
    assert launch[launch.index('--memory-swap') + 1] == '4g'
    assert 'tmux -S /tmp/aq-install.sock new-session' in launch[-1]
    assert cleanup[-1] == launch[launch.index('--name') + 1]
    assert not any('auth.json' in arg or 'claude-token' in arg for arg in launch)


def test_preflight_keeps_useful_engine_error_and_cleans_its_probe(tmp_path, monkeypatch):
    helper, package, config, commands = preflight_profile(tmp_path, monkeypatch, failure='Error: network helper is missing')
    with pytest.raises(ValueError, match='network helper is missing'):
        helper.sandbox_preflight(package, config, Path('/opt/podman'))
    assert commands[-1][-3:-1] == ['rm', '--force']


def test_preflight_refuses_missing_resource_controller_before_launch(tmp_path, monkeypatch):
    info = {'SecurityOptions': ['name=rootless'], 'CgroupVersion': '1', 'CgroupDriver': 'none'}
    helper, package, config, commands = preflight_profile(tmp_path, monkeypatch, info=info)
    with pytest.raises(ValueError, match='cgroup v2'):
        helper.sandbox_preflight(package, config, Path('/opt/podman'))
    assert not commands


def test_diagnostic_acknowledgement_retry_reuses_report(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    import uuid
    operation = str(uuid.uuid4()); requests = []; probes = []
    def request(_config, body, slot):
        assert slot == 2
        requests.append(body)
        if not body:
            return {'operation_id': operation, 'kind': 'sandbox_startup_v1'}
        if len(requests) == 2:
            raise OSError('simulated lost acknowledgement')
        return {'operation_id': None}
    def probe(_config):
        probes.append(True)
        return {'ok': False, 'phase': 'container_create', 'error': 'Synthetic engine refusal'}
    monkeypatch.setattr(helper, 'diagnostic_request', request)
    monkeypatch.setattr(helper, 'sandbox_diagnostics', probe)
    with pytest.raises(OSError):
        helper.sync_diagnostics_slot(package, config, 2)
    helper.sync_diagnostics_slot(package, config, 2)
    assert len(probes) == 1
    assert requests[1] == requests[3]


def test_receiver_update_checks_digest_and_updates_only_its_installed_helper(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    import hashlib
    import io
    target = config / 'login-sync.py'; target.write_text('old_receiver = True\n')
    monkeypatch.setattr(helper, '__file__', str(target))
    (config / 'hosted-install.json').write_text(json.dumps({'origin': 'https://worker.example'}))
    content = b"cat > temp <<'AQ_INSTALL_CONFIG'\nnew_receiver = True\nAQ_INSTALL_CONFIG\n"
    class Opener:
        def open(self, url, timeout):
            assert url.startswith('https://worker.example/install/')
            return io.BytesIO(content)
    monkeypatch.setattr(helper.urllib.request, 'build_opener', lambda *_args: Opener())
    with pytest.raises(ValueError, match='checksum'):
        helper.receiver_update(config, '0' * 64)
    assert target.read_text() == 'old_receiver = True\n'
    assert helper.receiver_update(config, hashlib.sha256(content).hexdigest())['ok']
    assert target.read_text() == 'new_receiver = True\n'
    assert target.stat().st_mode & 0o777 == 0o600


def test_remote_diagnostic_rejects_main_container_socket(tmp_path):
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'native.json').write_text(json.dumps({'executor': {'settings': {'host': {'docker_local': {'socket': '/run/podman/podman.sock'}}}}}))
    text = (config / 'service.toml').read_text()
    (config / 'service.toml').write_text('native_template = ' + json.dumps(str(config / 'native.json')) + '\n' + text)
    with pytest.raises(ValueError, match='sandbox socket'):
        helper.sandbox_diagnostics(config)


def test_control_only_update_preserves_native_and_runtime_files(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'hosted-install.json').write_text(json.dumps({'origin': 'https://worker.example', 'bundle_sha256': 'older-bundle'}))
    (config / 'slots/1').mkdir(parents=True)
    (config / 'slots/1/worker.toml').write_text('server_url = "https://worker.example"\ncredential = "synthetic-private"\n')
    (config / 'native.json').write_text('{"custom_existing_native":true}')
    (config / 'login-sync.py').write_text('old_receiver=True\n')
    before = {name: (config / name).read_bytes() for name in ('service.toml', 'native.json', 'slots/1/worker.toml', 'hosted-install.json')}
    commands = []
    monkeypatch.setattr(helper.subprocess, 'run', lambda args, **_kwargs: commands.append(args))
    helper.upgrade_controls(config, 'https://worker.example')
    assert (config / 'login-sync.py').read_text() == Path(helper.__file__).read_text()
    assert all((config / name).read_bytes() == raw for name, raw in before.items())
    assert commands[-1][2:4] == ['start', '--no-block']
    assert not any('aq-node@' in str(arg) for command in commands for arg in command)


def test_control_bootstrap_failure_reports_to_queued_diagnostic(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'hosted-install.json').write_text(json.dumps({'origin': 'https://worker.example'}))
    (config / 'slots/1').mkdir(parents=True)
    (config / 'slots/1/worker.toml').write_text('server_url = "https://worker.example"\n')
    requests = []
    def report(_config, body):
        requests.append(body)
        return {'operation_id': 'fixture-operation'}
    monkeypatch.setattr(helper, 'diagnostic_request', report)
    with pytest.raises(ValueError, match='receiver was not found'):
        helper.upgrade_controls(config, 'https://worker.example')
    assert requests[1]['report']['phase'] == 'controls_upgrade'
    assert requests[1]['report']['ok'] is False


def test_diagnostics_poll_survives_a_broken_service_configuration(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    (config / 'service.toml').write_text('not valid TOML')
    (config / 'slots/1').mkdir(parents=True)
    (config / 'slots/1/worker.toml').write_text('server_url="https://worker.example"\n')
    seen = []
    monkeypatch.setattr(helper, 'sync_slot', lambda *_args: None)
    monkeypatch.setattr(helper, 'sync_diagnostics_slot', lambda _package, _config, slot: seen.append(slot))
    helper.sync_logins(package, config)
    assert seen == [1]


def test_missing_iptables_is_restored_only_for_its_specific_startup_failure(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    reports = iter([{'ok': False, 'phase': 'container_start', 'error': 'Sandbox API 500: netavark: iptables: No such file or directory (os error 2)'}, {'ok': True, 'phase': 'complete', 'details': {}}])
    repaired = []
    monkeypatch.setattr(helper, 'sandbox_probe', lambda _config: next(reports))
    monkeypatch.setattr(helper, 'install_network_tools', lambda _config: repaired.append(True) or 'pinned-support')
    result = helper.sandbox_diagnostics(config)
    assert result['ok'] and result['details']['network_tools_restored'] == 'pinned-support'
    assert len(repaired) == 1


def test_unrelated_sandbox_failure_does_not_change_network_configuration(tmp_path, monkeypatch):
    helper = login_helper(); package, config = login_profile(tmp_path)
    failure = {'ok': False, 'phase': 'image', 'error': 'Image unavailable'}
    monkeypatch.setattr(helper, 'sandbox_probe', lambda _config: failure)
    monkeypatch.setattr(helper, 'install_network_tools', lambda _config: pytest.fail('Unrelated failure must not trigger repair'))
    assert helper.sandbox_diagnostics(config) == failure


def test_network_tools_install_is_private_pinned_and_restarts_only_aq_daemon(tmp_path, monkeypatch):
    import hashlib
    import io
    import tarfile
    from types import SimpleNamespace
    helper = login_helper(); package, config = login_profile(tmp_path)
    prefix = tmp_path / '.local/opt/aq-sandbox-podman'
    executable = prefix / 'podman-linux-amd64/usr/local/bin/podman'
    executable.parent.mkdir(parents=True); executable.write_text('fixture podman')
    text = (config / 'service.toml').read_text() + '\n[manual]\npodman_executable = ' + json.dumps(str(executable)) + '\n'
    (config / 'service.toml').write_text(text)
    (config / 'hosted-install.json').write_text(json.dumps({'origin': 'https://worker.example'}))
    files = {'bin/iptables': b'#!/bin/sh\nexit 0\n'}
    manifest = {'files': {'bin/iptables': {'sha256': hashlib.sha256(files['bin/iptables']).hexdigest()}}}
    files['manifest.json'] = json.dumps(manifest).encode()
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode='w:gz') as archive:
        for name, contents in files.items():
            entry = tarfile.TarInfo(name); entry.size = len(contents); entry.mode = 0o755 if name.startswith('bin/') else 0o644
            archive.addfile(entry, io.BytesIO(contents))
    body = stream.getvalue(); pin = hashlib.sha256(body).hexdigest()
    monkeypatch.setattr(helper, 'NETWORK_TOOLS_SHA256', pin)
    class Opener:
        def open(self, url, timeout):
            assert url == 'https://worker.example/install/' + pin + '.tar.gz'
            return io.BytesIO(body)
    monkeypatch.setattr(helper.urllib.request, 'build_opener', lambda *_args: Opener())
    commands = []
    def command(args, **_kwargs):
        commands.append(args)
        return SimpleNamespace(stdout='process\n')
    monkeypatch.setattr(helper.subprocess, 'run', command)
    monkeypatch.setenv('XDG_CONFIG_HOME', str(tmp_path / 'xdg'))
    original = Path.__dict__['home']; Path.home = classmethod(lambda _cls: tmp_path)
    try:
        assert helper.install_network_tools(config) == pin
    finally:
        Path.home = original
    assert (prefix / 'network-tools' / pin / 'bin/iptables').read_bytes() == files['bin/iptables']
    override = tmp_path / 'xdg/systemd/user/aq-sandbox-podman.service.d/30-aq-network-tools.conf'
    assert str(prefix / 'network-tools' / pin / 'bin') in override.read_text()
    assert commands[-1][-2:] == ['restart', 'aq-sandbox-podman.service']
    assert not any('sudo' in c or 'apt-get' in c or any('aq-node@' in a for a in c) for c in commands)


def test_host_companion_extracts_only_regular_allowlisted_files(tmp_path):
    import io
    import tarfile
    helper = login_helper()
    archive = tmp_path / 'bundle.tar.gz'
    def bundle(entries):
        with tarfile.open(archive, 'w:gz') as tar:
            for name, data, symlink in entries:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                if symlink:
                    info.type = tarfile.SYMTYPE
                    info.linkname = '/bin/sh'
                tar.addfile(info, io.BytesIO(data) if not symlink else None)
    bundle([('aq-worker', b'worker', False), ('tmux', b'tmux', False), ('service.toml', b'secret', False), ('../../escape', b'bad', False)])
    destination = tmp_path / 'safe'; destination.mkdir()
    helper.host_bundle_files(archive, destination)
    assert sorted(p.name for p in destination.iterdir()) == ['aq-worker', 'tmux']
    assert not (tmp_path / 'escape').exists()
    bundle([('aq-worker', b'', True), ('tmux', b'tmux', False)])
    rejected = tmp_path / 'rejected'; rejected.mkdir()
    with pytest.raises(ValueError, match='Invalid host terminal'):
        helper.host_bundle_files(archive, rejected)
    bundle([('aq-worker', b'first', False), ('aq-worker', b'second', False), ('tmux', b'tmux', False)])
    duplicate = tmp_path / 'duplicate'; duplicate.mkdir()
    with pytest.raises(ValueError, match='Invalid host terminal'):
        helper.host_bundle_files(archive, duplicate)
