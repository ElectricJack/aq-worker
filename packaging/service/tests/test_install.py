"""Install script checks: fake PATH stubs and a temporary HOME; nothing real is installed."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tomllib

import pytest

from install_fixtures import BINARIES, CODE, SCRIPT, downloadable_bundle, host
# Keep the installer suite and its individual scenarios selectable from this module.
from install_service_scenarios import (
    login_helper,
    test_server_login_enables_manual_sessions_and_remains_private,
    test_invalid_delivery_writes_no_secrets,
    test_receipt_retry_does_not_restart_twice,
    test_manual_terminal_companion_is_installed_and_kept_alive_without_restarting_worker,
    test_unconfigured_provider_does_not_start_terminal_companion,
    test_connected_receiver_repairs_missing_terminal_companion_even_without_a_new_login,
    test_login_unit_uses_profile_and_persistent_helper,
    test_retired_primary_does_not_prevent_sibling_delivery,
    test_preflight_exercises_real_launch_limits_bridge_and_tmux,
    test_preflight_keeps_useful_engine_error_and_cleans_its_probe,
    test_preflight_refuses_missing_resource_controller_before_launch,
    test_diagnostic_acknowledgement_retry_reuses_report,
    test_receiver_update_checks_digest_and_updates_only_its_installed_helper,
    test_remote_diagnostic_rejects_main_container_socket,
    test_control_only_update_preserves_native_and_runtime_files,
    test_control_bootstrap_failure_reports_to_queued_diagnostic,
    test_diagnostics_poll_survives_a_broken_service_configuration,
    test_missing_iptables_is_restored_only_for_its_specific_startup_failure,
    test_unrelated_sandbox_failure_does_not_change_network_configuration,
    test_network_tools_install_is_private_pinned_and_restarts_only_aq_daemon,
    test_host_companion_extracts_only_regular_allowlisted_files,
)


def with_socket(host):
    path = host["runtime"] / "aq-sandbox/podman.sock"
    path.unlink(missing_ok=True)
    daemon = socket.socket(socket.AF_UNIX)
    daemon.bind(str(path))
    return daemon


def run(host, *args, env=None, script=SCRIPT):
    full = {**host["env"], **(env or {})}
    return subprocess.run(
        ["bash", str(script), "--server", "https://machine.example",
         "--bundle", str(host["bundle"]), *args],
        env=full, input=CODE + "\n", capture_output=True, text=True, timeout=20)


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


def test_installs_binaries_and_calls_enroll_install_status_in_order(host):
    with with_socket(host):
        result = run(host, "--profile", "p1", "--slots", "3", "--name", "box", "--machine-key", "box-1")
    assert result.returncode == 0, result.stderr
    runtime = Path(host["env"]["HOME"]) / ".local/share/agent-q/runtime"
    for name in BINARIES:
        assert (runtime / name).stat().st_mode & 0o777 == 0o755
    link = Path(host["env"]["HOME"]) / ".local/bin/aq-worker"
    assert link.is_symlink() and link.resolve() == (runtime / "aq-worker").resolve()
    steps = [c["args"][:2] for c in worker_calls(host)]
    assert steps == [["service", "enroll"], ["service", "install"], ["service", "status"]]
    enroll = worker_calls(host)[0]["args"]
    assert enroll[enroll.index("--nodes") + 1] == "3"
    assert enroll[enroll.index("--name") + 1] == "box"
    assert enroll[enroll.index("--machine-key") + 1] == "box-1"
    assert enroll[enroll.index("--aq-git") + 1] == str(runtime / "aq-git")
    assert not any("measurement" in arg for arg in enroll)
    assert "allow_enrolled_nodes" in result.stdout


def test_defaults_template_and_repositories_from_the_bundle(host):
    with with_socket(host):
        result = run(host)
    assert result.returncode == 0, result.stderr
    enroll = worker_calls(host)[0]["args"]
    assert enroll[enroll.index("--native-template") + 1] == str(host["bundle"] / "native.json")
    assert enroll[enroll.index("--repositories") + 1] == str(host["bundle"] / "repositories.json")
    assert enroll[enroll.index("--profile") + 1] == "local"
    assert enroll[enroll.index("--nodes") + 1] == "1"


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
    archive, extracted, codex_composite = downloadable_bundle(host, monkeypatch)
    template = json.loads((extracted / "native.json").read_text())
    assert "workspace_bind_paths" not in template["executor"]["settings"]["host"]["docker_local"]
    work = host["tmp"] / "prepared"
    work.mkdir()
    monkeypatch.setenv("HOME", str(host["home"]))
    config = host["home"] / ".config/agent-q/service/local"
    subprocess.run(
        [sys.executable, extracted / "worker-install-config.py", "prepare",
         extracted, work, "https://worker.example", host["tmp"] / "podman.sock",
         config, archive.name.removesuffix(".tar.gz")],
        check=True,
    )
    prepared = json.loads((work / "native.json").read_text())
    codex = prepared["executor"]["settings"]["codex"]
    assert codex == {
        "composite_definition": str(extracted / "codex-composite.json"),
        "runtime_bundle": str(extracted / "codex-bundle"),
    }
    assert Path(codex["composite_definition"]).read_bytes() == codex_composite.read_bytes()
    assert Path(codex["composite_definition"]).stat().st_mode & 0o777 == 0o600
    assert (Path(codex["runtime_bundle"]) / "codex-path/rg").stat().st_mode & 0o111
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


def test_an_active_container_worker_is_turned_off_and_its_configuration_kept(host):
    with with_socket(host):
        quiet = run(host)
        loud = run(host, env={"STUB_ACTIVE": "aq-worker@local.service aq-sandbox-podman.service"})
    assert quiet.returncode == 0 and "aq-worker@local" not in quiet.stdout + quiet.stderr
    assert not [c for c in calls(host) if c["name"] == "systemctl" and "disable" in c["args"]][:-1], "only the active one"
    assert loud.returncode == 0, loud.stderr
    assert "Turning off the container worker aq-worker@local.service" in loud.stdout
    assert "systemctl --user enable --now aq-worker@local.service" in loud.stdout
    disabled = [c["args"] for c in calls(host) if c["name"] == "systemctl" and "disable" in c["args"]]
    assert disabled == [["--user", "disable", "--now", "aq-worker@local.service"]]


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
def test_hosted_install_writes_manual_sessions_with_their_claude_token_file(tmp_path, manual_sessions, with_token):
    (tmp_path / "bootstrap.json").write_text(json.dumps({
        "manual_sessions": manual_sessions, "source_slugs": {"repo": "owner/repo"},
    }))
    service = tmp_path / "service.toml"
    service.write_text("slots = 1\n")
    argv = [sys.executable, str(SCRIPT.with_name("worker-install-config.py")),
            "configure", str(tmp_path), str(tmp_path), "https://worker.example",
            "/usr/bin/podman", "1", "a" * 64]
    if with_token:
        argv.append("/private/claude-token")
    for _ in range(2):
        result = subprocess.run(argv, capture_output=True, text=True, timeout=20)
        assert result.returncode == 0, result.stderr
        written = tomllib.loads(service.read_text())
        assert "measurement" not in written
        if manual_sessions and with_token:
            assert written["manual"] == {
                "podman_executable": "/usr/bin/podman",
                "claude_token_file": "/private/claude-token", "source_slugs": {"repo": "owner/repo"},
            }
            assert "Use Push Claude token" not in result.stdout
        else:
            assert written == {"slots": 1}
            assert "Use Push Claude token" in result.stdout


def test_hosted_install_keeps_an_existing_manual_section_and_enrollment(tmp_path):
    (tmp_path / "bootstrap.json").write_text(json.dumps({
        "manual_sessions": True, "source_slugs": {},
    }))
    service = tmp_path / "service.toml"
    original = ('slots = 1\nmachine_key = "existing-pc"\n\n[manual]\npodman_executable = "/usr/bin/podman"\n'
                'max_slots = 1\nclaude_token_file = "/private/first"\n\n[manual.source_slugs]\n')
    service.write_text(original)
    enrollment = tmp_path / "worker.toml"
    enrollment.write_text('credential = "existing-enrollment"\n')
    for _ in range(2):
        result = subprocess.run(
            [sys.executable, str(SCRIPT.with_name("worker-install-config.py")),
             "configure", str(tmp_path), str(tmp_path), "https://worker.example",
             "/usr/bin/podman", "1", "a" * 64, "/private/second"],
            capture_output=True, text=True, timeout=20,
        )
        assert result.returncode == 0, result.stderr
        assert service.read_text() == original
        assert enrollment.read_text() == 'credential = "existing-enrollment"\n'


def hosted_profile(tmp_path, digest):
    """A profile enrolled from the hosted installer's bundle `digest`, and that bundle's package."""
    package, config, runtime = tmp_path / ("pkg-" + digest[:4]), tmp_path / "config", tmp_path / "home/.local/share/agent-q/runtime"
    for directory in (package, config / "slots/1", runtime):
        directory.mkdir(parents=True, exist_ok=True)
    template = {
        "adapter_manifest": "/adapter.json", "inventory_definition": "/inventory.json",
        "executor": {"composite_definition": "/composite.json", "settings": {"host": {
            "root": "/bundle/host",
            "helper": {"path": "/bundle/aq-session-host"}, "tmux": {"path": "/bundle/tmux"},
            "docker_local": {"socket": "/bundle/podman.sock", "templates": [{"image": "registry.invalid/owner/image", "digest": "sha256:" + "0" * 64}]},
        }}},
    }
    (package / "native.json").write_text(json.dumps(template))
    for name in ("adapter.json", "composite.json", "inventory.json"):
        (package / name).write_text("{}")
        (package / name).chmod(0o644)
    for name in ("aq-worker", "aq-session-host", "aq-agent", "aq-git"):
        (package / name).write_text(name + digest)
    return package, config, runtime


def run_config(tmp_path, *args):
    return subprocess.run(
        [sys.executable, str(SCRIPT.with_name("worker-install-config.py")), *map(str, args)],
        capture_output=True, text=True, timeout=20, env={**os.environ, "HOME": str(tmp_path / "home")},
    )


def test_hosted_workspace_mapping_uses_the_actual_profile_state(tmp_path, monkeypatch):
    helper = login_helper()
    package, config, _ = hosted_profile(tmp_path, "a" * 64)
    source = json.loads((package / "native.json").read_text())
    source["node_git"] = {"repositories": "/bundle/node_git"}
    docker = source["executor"]["settings"]["host"]["docker_local"]
    docker["workspace_bind_paths"] = {"worker_root": "/builder", "daemon_root": "/builder"}
    (package / "native.json").write_text(json.dumps(source))
    state_home = tmp_path / "custom-state-home"
    monkeypatch.setenv("XDG_STATE_HOME", str(state_home))

    def mapping():
        template, _ = helper.prepared_template(package, config, "https://worker.example", tmp_path / "podman.sock")
        return template["executor"]["settings"]["host"]["docker_local"].get("workspace_bind_paths")

    expected = str(state_home / "agent-q/service" / config.name)
    assert mapping() == {"worker_root": expected, "daemon_root": expected}
    state = str(tmp_path / "existing-custom-state")
    (config / "service.toml").write_text(f"state_root = {json.dumps(state)}\n")
    assert mapping() == {"worker_root": state, "daemon_root": state}
    (config / "service.toml").write_text('state_root = "/"\n')
    assert mapping() is None


def test_a_newer_bundle_from_the_same_server_updates_an_enrolled_profile(tmp_path):
    origin, old, new = "https://worker.example", "a" * 64, "b" * 64
    package, config, runtime = hosted_profile(tmp_path, old)
    work = tmp_path / "work"
    work.mkdir()
    # First install: prepare, then what aq-install.sh and enrollment leave behind.
    first = run_config(tmp_path, "prepare", package, work, origin, tmp_path / "podman.sock", config, old)
    assert first.returncode == 0, first.stderr
    (config / "native.json").write_text((work / "native.json").read_text())
    (config / "service.toml").write_text("slots = 1\n")
    (config / "slots/1/worker.toml").write_text(f'server_url = "{origin}"\n')
    (config / "hosted-install.json").write_text(json.dumps({"origin": origin, "bundle_sha256": old}))
    for name in ("aq-worker", "aq-session-host", "aq-agent", "aq-git"):
        (runtime / name).write_text((package / name).read_text())
    # A newer bundle from another server is refused.
    newer, _, _ = hosted_profile(tmp_path, new)
    other = run_config(tmp_path, "prepare", newer, work, "https://other.example", tmp_path / "podman.sock", config, new)
    assert other.returncode != 0 and "another server" in other.stderr
    # From the same server it is an update: allowed although the runtime and template differ.
    update = run_config(tmp_path, "prepare", newer, work, origin, tmp_path / "podman.sock", config, new)
    assert update.returncode == 0, update.stderr
    assert str(newer) not in (config / "native.json").read_text(), "the template changes only once the binaries are in place"
    (newer / "bootstrap.json").write_text(json.dumps({"manual_sessions": False, "source_slugs": {}}))
    done = run_config(tmp_path, "configure", newer, config, origin, "/usr/bin/podman", "1", new)
    assert done.returncode == 0, done.stderr
    assert "updated" in done.stdout
    assert str(newer / "aq-session-host") in (config / "native.json").read_text()
    assert not (config / "native.json.update").exists()
    # What the worker checks when it loads the template: a private state directory that holds the git root.
    template = json.loads((config / "native.json").read_text())
    state = Path(template["state_dir"])
    assert state == config / "state" and state.is_dir() and state.stat().st_mode & 0o777 == 0o700
    assert Path(template["git_root"]).parent == state
    for key in (template["adapter_manifest"], template["inventory_definition"], template["executor"]["composite_definition"]):
        assert Path(key).stat().st_mode & 0o777 == 0o600, "the worker reads its definitions only as private files"
    # A machine installed by an earlier installer mends itself on the receiver's next pass.
    helper = login_helper()
    (config / "service.toml").write_text(f'slots = 1\nnative_template = "{config / "native.json"}"\n')
    Path(template["adapter_manifest"]).chmod(0o644)
    shutil.rmtree(state)
    helper.repair_native_files(config)
    assert Path(template["adapter_manifest"]).stat().st_mode & 0o777 == 0o600
    assert state.is_dir() and state.stat().st_mode & 0o777 == 0o700
    assert json.loads((config / "hosted-install.json").read_text()) == {"origin": origin, "bundle_sha256": new}
    assert (config / "slots/1/worker.toml").read_text() == f'server_url = "{origin}"\n', "the enrollment is kept"


def test_an_update_stops_the_running_workers_before_replacing_their_files(host):
    profile = host["home"] / ".config/agent-q/service/local"
    profile.mkdir(parents=True)
    (profile / "service.toml").write_text("")
    runtime = host["home"] / ".local/share/agent-q/runtime"
    runtime.mkdir(parents=True)
    for name in ("aq-worker", "aq-session-host", "aq-agent", "aq-git"):
        (runtime / name).write_text("an older build of " + name)
    active = "aq-node@local.service aq-node@other.service"
    with with_socket(host):
        result = run(host, env={"STUB_ACTIVE": active})
    assert result.returncode == 0, result.stderr
    assert (runtime / "aq-agent").read_bytes() == (host["bundle"] / "aq-agent").read_bytes()
    systemd = [c["args"] for c in calls(host) if c["name"] == "systemctl"]
    stops = [a[-1] for a in systemd if "stop" in a]
    starts = [a[-1] for a in systemd if "start" in a]
    assert stops == ["aq-node@local.service", "aq-node@other.service"], result.stdout
    assert starts == ["aq-node@other.service"], "this profile's service is started by `service install`"
    order = [c["name"] + " " + " ".join(c["args"][:2]) for c in calls(host)]
    assert order.index("systemctl --user stop") < order.index("aq-worker service install")
    # The same bundle again changes nothing, so nothing is stopped.
    with with_socket(host):
        again = run(host, env={"STUB_ACTIVE": active})
    assert again.returncode == 0, again.stderr
    assert len([c for c in calls(host) if c["name"] == "systemctl" and "stop" in c["args"]]) == 2



def test_the_remote_check_reports_what_the_worker_service_says_without_secrets(tmp_path, monkeypatch):
    helper = login_helper()
    config = tmp_path / "local"
    (config / "state").mkdir(parents=True, mode=0o700)
    present = tmp_path / "present"
    present.write_text("")
    native = {"state_dir": str(config / "state"), "git_root": str(config / "state/git"), "adapter_manifest": str(present),
              "inventory_definition": str(tmp_path / "absent.json"),
              "executor": {"composite_definition": str(present), "settings": {"host": {
                  "docker_local": {"socket": str(present)},
                  "helper": {"path": str(present)}, "tmux": {"path": str(present)}}}}}
    (config / "native.json").write_text(json.dumps(native))
    (config / "service.toml").write_text(f'slots = 1\nnative_template = "{config / "native.json"}"\n')
    log = "attempt failed: No such file or directory\ntoken sk-ant-oat01-abcDEF_123 seen\n" + "x" * 500 + "\n"
    def fake_run(argv, **_):
        out = {"systemctl": "active\n", "journalctl": log}.get(Path(argv[0]).name, "aq-worker 0.5.1+abc\n")
        return subprocess.CompletedProcess(argv, 0, stdout=out, stderr="")
    monkeypatch.setattr(helper.subprocess, "run", fake_run)
    report = helper.worker_service_report(config)
    assert report["active"] == "active" and report["version"] == "aq-worker 0.5.1+abc"
    assert "measurement" not in report
    assert report["missing"] == ["inventory_definition"]
    assert report["state_dir"] == {"mode": "0o700", "holds_git_root": True}
    assert report["log"][0] == "attempt failed: No such file or directory"
    assert report["log"][1] == "token [redacted] seen" and len(report["log"][2]) == 240
    assert len(json.dumps(report)) < 12000



@pytest.mark.parametrize("key", ["nodes", "slots"])
def test_hosted_install_node_count_preserves_each_enrollment(tmp_path, key):
    (tmp_path / "bootstrap.json").write_text(json.dumps({"manual_sessions": False, "source_slugs": {}}))
    (tmp_path / "service.toml").write_text(f"{key} = 2\n")
    for node in (1, 2):
        path = tmp_path / f"slots/{node}/worker.toml"
        path.parent.mkdir(parents=True)
        path.write_text(f"credential = 'node-{node}'\n")
    argv = [sys.executable, str(SCRIPT.with_name("worker-install-config.py")),
            "configure", str(tmp_path), str(tmp_path), "https://worker.example",
            "/usr/bin/podman", "2", "a" * 64]
    result = subprocess.run(argv, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert tomllib.loads((tmp_path / "service.toml").read_text()) == {key: 2}
    for node in (1, 2):
        assert (tmp_path / f"slots/{node}/worker.toml").read_text() == f"credential = 'node-{node}'\n"


def test_a_node_git_section_gets_one_repository_directory_per_profile(tmp_path, monkeypatch):
    """Spec 0016 §5.3: a node_git template keeps node_git and gets the profile's
    node-wide repository directory; a template without it gets none."""
    module = login_helper()
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    for node_git in (None, {"repositories": "/bundle/node_git", "git": "/usr/bin/git"}):
        root = tmp_path / ("with" if node_git else "without")
        package, work, config = (root / name for name in ("package", "work", "config"))
        for directory in (package, work, config):
            directory.mkdir(parents=True)
        for name in ("adapter.json", "composite.json", "inventory.json"):
            (package / name).write_text("{}")
        native = {"executor": {"settings": {"host": {
            "docker_local": {"templates": [{"image": "registry.example/aq/sandbox"}]}, "helper": {}, "tmux": {},
        }}}}
        if node_git:
            native["node_git"] = dict(node_git)
        (package / "native.json").write_text(json.dumps(native))
        module.prepare(package, work, "https://worker.example", tmp_path / "podman.sock", config, "a" * 64)
        written = json.loads((work / "native.json").read_text())
        if node_git:
            assert written["node_git"] == {"repositories": str(config / "state/node-git"), "git": "/usr/bin/git"}
        else:
            assert "node_git" not in written


def test_installer_waits_for_a_fresh_inventory_report(monkeypatch, tmp_path):
    spec = importlib.util.spec_from_file_location(
        "worker_install_config", SCRIPT.with_name("worker-install-config.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    stale = {"recorded_at_ms": 80, "expires_at_ms": 110}
    fresh = {"recorded_at_ms": 95, "expires_at_ms": 110}
    reports = iter([stale, fresh])
    monkeypatch.setattr(
        module, "inventory_status",
        lambda _: {"server_time_ms": 100, "accepted": next(reports)},
    )
    import time

    monkeypatch.setattr(time, "sleep", lambda _: None)
    module.wait_for_inventory(tmp_path, 90)
    assert next(reports, None) is None
