"""Hosted installer login delivery, service diagnostics, and control repair scenarios."""
import json
from pathlib import Path

import pytest

from install_fixtures import SCRIPT


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
    (package / "bootstrap.json").write_text(json.dumps({"manual_sessions": True}))
    (config / "login-sync.json").write_text(json.dumps({"podman": "/opt/podman"}))
    (config / "service.toml").write_text('profile = "local"\nslots = 1\n')
    return package, config


def test_server_login_enables_manual_sessions_and_remains_private(tmp_path):
    helper = login_helper()
    package, config = login_profile(tmp_path)
    logins = [{"provider": "anthropic", "material": "synthetic-claude"}]
    assert helper.apply_logins(package, config, logins)
    parsed = helper.read_toml(config / "service.toml")
    assert "source_slugs" not in parsed["manual"] and "repositories" not in parsed
    assert "measurement" not in parsed
    assert parsed["manual"]["claude_token_file"] == str(config / "provider-auth/claude-token")
    # Codex is installed without a login file: its login is leased from the server.
    assert parsed["manual"]["codex"] == {"executable": "/opt/aq/bin/codex", "runtime_bundle": str(package / "codex-bundle")}
    for path in (config / "provider-auth").iterdir():
        assert path.stat().st_mode & 0o777 == 0o600
    assert (config / "provider-auth").stat().st_mode & 0o777 == 0o700
    assert not helper.apply_logins(package, config, logins)


def test_invalid_delivery_writes_no_secrets(tmp_path):
    helper = login_helper(); package, config = login_profile(tmp_path)
    codex = json.dumps({"auth_mode": "chatgpt", "tokens": {"access_token": "a", "refresh_token": "r", "id_token": "i", "account_id": "acct"}})
    # A Codex login is never installed on a machine, with or without the Claude token.
    for logins in ([], [{"provider": "anthropic", "material": "bad\nvalue"}], [{"provider": "openai", "material": codex}], [{"provider": "anthropic", "material": "synthetic"}, {"provider": "openai", "material": codex}]):
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
    updated = []
    monkeypatch.setattr(helper, 'worker_binary_update', lambda c, text, origin: updated.append(origin) or {'version': 'new'})
    report = helper.receiver_update(config, hashlib.sha256(content).hexdigest())
    assert report['ok'] and report['details']['worker'] == {'version': 'new'}
    assert updated == ['https://worker.example']
    assert target.read_text() == 'new_receiver = True\n'
    assert target.stat().st_mode & 0o777 == 0o600
    # A worker update that fails is reported; the receiver update itself stands.
    monkeypatch.undo(); monkeypatch.setattr(helper, '__file__', str(target))
    monkeypatch.setattr(helper.urllib.request, 'build_opener', lambda *_args: Opener())
    report = helper.receiver_update(config, hashlib.sha256(content).hexdigest())
    assert report['ok'] and 'names no worker bundle' in report['details']['worker']['error']


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
