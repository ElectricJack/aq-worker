"""Credential-free packaging boundaries; actual image smoke is a separate check."""
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import subprocess
import tarfile

import pytest

ROOT = Path(__file__).resolve().parents[3]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


packager = module('packager', ROOT / 'scripts/package-native-runtime.py')
smoke = module('smoke', ROOT / 'packaging/native-runtime/smoke.py')


def test_copied_input_requires_exact_pin_and_regular_file(tmp_path):
    source = tmp_path / 'input'
    source.write_bytes(b'public input')
    pin = {'sha256': hashlib.sha256(source.read_bytes()).hexdigest(), 'bytes': 12}
    packager.checked_copy(source, tmp_path / 'good', pin)
    with pytest.raises(ValueError, match='checksum mismatch'):
        packager.checked_copy(source, tmp_path / 'bad', dict(pin, sha256='0' * 64))
    with pytest.raises(ValueError, match='size mismatch'):
        packager.checked_copy(source, tmp_path / 'size', dict(pin, bytes=10))
    link = tmp_path / 'link'
    link.symlink_to(source)
    with pytest.raises(ValueError, match='regular input'):
        packager.checked_copy(link, tmp_path / 'linked', pin)


def test_context_uses_committed_allowlist_and_never_walks_input_or_worktree(tmp_path, monkeypatch):
    repo = tmp_path / 'repo'
    repo.mkdir()
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    (repo / 'Cargo.toml').write_text('[workspace]\n')
    (repo / 'Cargo.lock').write_text('version = 4\n')
    for crate in ('aq-worker', 'aq-agent-client', 'aq-session-host'):
        directory = repo / 'crates' / crate
        directory.mkdir(parents=True)
        (directory / 'Cargo.toml').write_text('[package]\nversion = "1.2.3"\n')
    (repo / 'vendor').mkdir()
    (repo / 'vendor/public').write_text('public')
    protocol = repo / 'protocol/aq-admission'
    protocol.mkdir(parents=True)
    (protocol / 'Cargo.toml').write_text('[package]\nname = "aq-admission"\nversion = "0.3.2"\n')
    embedded = repo / 'packaging/sandbox'
    embedded.mkdir(parents=True)
    for name in ('template_tool.py', 'credential_scan.py'):
        (embedded / name).write_text('# committed embedded tool\n')
    packaging = repo / packager.PACKAGE
    packaging.mkdir(parents=True)
    inputs = tmp_path / 'inputs'
    inputs.mkdir()
    (inputs / 'asset').write_bytes(b'public')
    (inputs / 'auth.json').write_text('INPUT SECRET')
    names = ('asset', *smoke.CODEX_COMPANIONS)
    for name in smoke.CODEX_COMPANIONS:
        path = inputs / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'public')
    (inputs / 'codex-resources/auth.json').write_text('NESTED SECRET')
    pins = {'inputs': {name: {'sha256': hashlib.sha256(b'public').hexdigest()} for name in names}}
    (packaging / 'pins.json').write_text(json.dumps(pins))
    for name in ('Dockerfile', 'smoke.py'):
        (packaging / name).write_text('public')
    # Even tracked secrets outside the build allowlist must never enter context.
    (repo / '.aq').mkdir()
    (repo / '.aq/private-token').write_text('TRACKED SECRET')
    (repo / 'auth.json').write_text('TRACKED AUTH')
    subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
    subprocess.run(['git', '-C', str(repo), 'commit', '-qm', 'fixture'], check=True)
    (repo / 'crates/aq-worker/auth.json').write_text('UNTRACKED SECRET')
    (repo / 'Cargo.toml').write_text('DIRTY SECRET')
    monkeypatch.setattr(packager, 'ROOT', repo)
    output = tmp_path / 'context'
    result = packager.package('HEAD', inputs, output)
    assert result['versions'] == {name: '1.2.3' for name in ('aq-worker', 'aq-agent', 'aq-session-host')}
    files = [path for path in output.rglob('*') if path.is_file()]
    assert {str(path.relative_to(output)) for path in files} == {
        'worker-source.tar', 'Dockerfile', 'smoke.py', 'pins.json', 'build.json', 'SHA256SUMS', *names}
    assert all(b'SECRET' not in path.read_bytes() and b'TRACKED AUTH' not in path.read_bytes()
               for path in files)
    subprocess.run(['sha256sum', '-c', 'SHA256SUMS'], cwd=output, check=True, capture_output=True)
    with tarfile.open(output / 'worker-source.tar') as archive:
        assert archive.extractfile('protocol/aq-admission/Cargo.toml').read() == (protocol / 'Cargo.toml').read_bytes()
        for name in ('template_tool.py', 'credential_scan.py'):
            assert archive.extractfile(f'packaging/sandbox/{name}').read() == (embedded / name).read_bytes()
    with pytest.raises(ValueError, match='already exists'):
        packager.package('HEAD', inputs, output)
    (inputs / 'codex-code-mode-host').unlink()
    with pytest.raises(FileNotFoundError, match='codex-code-mode-host'):
        packager.package('HEAD', inputs, tmp_path / 'incomplete')
    assert not (tmp_path / 'incomplete').exists()


@pytest.fixture
def codex_layout(tmp_path, monkeypatch):
    monkeypatch.setattr(smoke, 'IMAGE', tmp_path)
    header = bytearray(64)
    header[:6] = b'\x7fELF\x02\x01'
    struct.pack_into('<H', header, 18, 62)
    struct.pack_into('<HH', header, 54, 56, 0)
    pins = {'inputs': {}}
    for name in ('codex', *smoke.CODEX_COMPANIONS):
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(header)
        path.chmod(0o755)
        pins['inputs'][name] = {'sha256': hashlib.sha256(header).hexdigest(), 'bytes': len(header)}
    return pins


def test_codex_preflight_accepts_complete_pinned_layout(codex_layout):
    smoke.check_codex_layout(codex_layout)


@pytest.mark.parametrize('name', smoke.CODEX_COMPANIONS)
@pytest.mark.parametrize('damage', ['missing', 'modified', 'not_executable'])
def test_codex_preflight_refuses_incomplete_or_changed_companions(codex_layout, name, damage):
    path = smoke.IMAGE / name
    if damage == 'missing':
        path.unlink()
    elif damage == 'modified':
        path.write_bytes(b'wrong release')
    else:
        path.chmod(0o644)
    with pytest.raises(ValueError, match=name):
        smoke.check_codex_layout(codex_layout)


def test_image_copies_and_pins_the_codex_tool_closure():
    packaging = ROOT / 'packaging/native-runtime'
    pins = json.loads((packaging / 'pins.json').read_text())
    dockerfile = (packaging / 'Dockerfile').read_text()
    for name in smoke.CODEX_COMPANIONS:
        assert f'COPY --from=build /inputs/{name} /opt/aq-runtime/{name}\n' in dockerfile
        assert pins['inputs'][name]['version'] == pins['inputs']['codex']['version']
        assert len(pins['inputs'][name]['sha256']) == 64
        assert pins['inputs'][name]['bytes'] > 0
    assert 'ENV PATH="/opt/aq-runtime/codex-path:' in dockerfile


def test_wrong_architecture_or_wrapper_is_not_an_elf(tmp_path):
    path = tmp_path / 'codex'
    for data in (b'#!/bin/sh\n', b'\x7fELF\x02\x01' + bytes(58)):
        path.write_bytes(data)
        with pytest.raises(ValueError, match='not ELF64|not x86-64'):
            smoke.elf_interpreter(path)


def test_smoke_refuses_missing_library_even_if_ldd_exits_zero(tmp_path, monkeypatch):
    # Regression: tmux -V failed with libevent_core missing in I0's first image.
    result = subprocess.CompletedProcess(['ldd'], 0, 'libevent_core-2.1.so.7 => not found', '')
    monkeypatch.setattr(smoke, 'elf_interpreter', lambda path: True)
    monkeypatch.setattr(smoke.subprocess, 'run', lambda *args, **kwargs: result)
    with pytest.raises(ValueError, match='missing shared library'):
        smoke.check_dependencies(tmp_path / 'tmux', {})
