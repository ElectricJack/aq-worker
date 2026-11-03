"""Credential-free packaging boundaries; actual image smoke is a separate check."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess

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
    packaging = repo / packager.PACKAGE
    packaging.mkdir(parents=True)
    inputs = tmp_path / 'inputs'
    inputs.mkdir()
    (inputs / 'asset').write_bytes(b'public')
    (inputs / 'auth.json').write_text('INPUT SECRET')
    pins = {'inputs': {'asset': {'sha256': hashlib.sha256(b'public').hexdigest()}}}
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
    assert set(path.name for path in output.iterdir()) == {
        'worker-source.tar', 'Dockerfile', 'smoke.py', 'pins.json', 'asset', 'build.json', 'SHA256SUMS'}
    assert all(b'SECRET' not in path.read_bytes() and b'TRACKED AUTH' not in path.read_bytes()
               for path in output.iterdir())
    subprocess.run(['sha256sum', '-c', 'SHA256SUMS'], cwd=output, check=True, capture_output=True)
    with pytest.raises(ValueError, match='already exists'):
        packager.package('HEAD', inputs, output)


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
