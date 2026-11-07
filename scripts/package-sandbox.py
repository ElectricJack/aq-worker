#!/usr/bin/env python3
"""Export isolated sandbox image contexts from a committed worker revision."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = 'packaging/sandbox/'


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def checked_copy(source, destination, expected):
    # Refuse symlinks, directories and devices before copying any public input.
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError(f'not a regular input: {source.name}')
        with destination.open('xb') as output:
            shutil.copyfileobj(stream, output)
    if digest(destination) != expected:
        raise ValueError(f'input checksum mismatch: {source.name}')
    destination.chmod(0o644)


def snapshot_sources(timestamp):
    return (
        'Types: deb\n'
        f'URIs: http://snapshot.debian.org/archive/debian/{timestamp}/\n'
        'Suites: bookworm bookworm-updates\nComponents: main\n'
        'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n\n'
        'Types: deb\n'
        f'URIs: http://snapshot.debian.org/archive/debian-security/{timestamp}/\n'
        'Suites: bookworm-security\nComponents: main\n'
        'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n'
    )


def package(revision, inputs, destination, *, pins=None):
    if destination.exists():
        raise ValueError('destination already exists; use a fresh context directory')
    revision = git('rev-parse', '--verify', revision + '^{commit}').decode().strip()
    read = lambda path: git('show', revision + ':' + path)
    pins = pins if pins is not None else json.loads(read(PACKAGE + 'pins.json'))
    if pins['schema_version'] != 1 or pins['platform'] != 'linux/amd64':
        raise ValueError('unsupported pins schema/platform')
    for field in ('base', 'builder'):
        if not re.fullmatch(r'[a-z0-9./:-]+@sha256:[0-9a-f]{64}', pins[field]):
            raise ValueError(f'{field} must be digest-pinned')
    if not re.fullmatch(r'\d{8}T\d{6}Z', pins['debian_snapshot']):
        raise ValueError('invalid Debian snapshot timestamp')
    required = {'git.tar.xz', 'aq-git', 'aqv2', 'codex', 'claude', 'opencode', 'gemini.tgz', 'node.tar.xz'}
    if set(pins['inputs']) != required:
        raise ValueError('unsupported public input set')
    for name, pin in pins['inputs'].items():
        if not re.fullmatch(r'[0-9a-f]{64}', pin['sha256']):
            raise ValueError(f'invalid input SHA-256: {name}')
        if (inputs / name).is_symlink():
            raise ValueError(f'not a regular input: {name}')
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.sandbox-', dir=destination.parent) as temporary:
        stage = Path(temporary)
        os_context, runtime = stage / 'os', stage / 'runtime'
        os_context.mkdir()
        runtime.mkdir()
        for name in sorted(required):
            context = os_context if name == 'git.tar.xz' else runtime
            checked_copy(inputs / name, context / name, pins['inputs'][name]['sha256'])
        replacements = {'@@BASE@@': pins['base'], '@@BUILDER@@': pins['builder']}
        for kind, context in (('os', os_context), ('runtime', runtime)):
            definition = read(PACKAGE + 'Dockerfile.' + kind).decode()
            for token, value in replacements.items():
                definition = definition.replace(token, value)
            (context / 'Dockerfile').write_text(definition)
            (context / 'snapshot.sources').write_text(snapshot_sources(pins['debian_snapshot']))
            names = ['git.tar.xz'] if kind == 'os' else ['aq-git', 'aqv2', 'codex', 'claude', 'opencode', 'gemini.tgz', 'node.tar.xz']
            (context / 'SHA256SUMS').write_text(''.join(
                pins['inputs'][name]['sha256'] + '  ' + name + '\n' for name in names))
        (runtime / 'pins.json').write_text(json.dumps(pins, sort_keys=True, indent=2) + '\n')
        (runtime / 'bundle_manifest.py').write_bytes(read(PACKAGE + 'bundle_manifest.py'))
        # Git archive exports committed paths only; no worktree traversal or homes.
        with (runtime / 'worker-source.tar').open('xb') as output:
            subprocess.run(['git', 'archive', '--format=tar', revision, 'Cargo.toml', 'Cargo.lock', 'crates', 'vendor',
                            PACKAGE + 'template_tool.py', PACKAGE + 'credential_scan.py', 'protocol/aq-admission'],
                           cwd=ROOT, stdout=output, check=True)
        archive_digest = digest(runtime / 'worker-source.tar')
        source = {
            'revision': revision,
            'archive_sha256': archive_digest,
            'runner_version': tomllib.loads(read('crates/aq-worker/Cargo.toml').decode())['package']['version'],
            'session_host_version': tomllib.loads(read('crates/aq-session-host/Cargo.toml').decode())['package']['version'],
        }
        (runtime / 'source.json').write_text(json.dumps(source, sort_keys=True, indent=2) + '\n')
        (runtime / 'worker-source.sha256').write_text(archive_digest + '  worker-source.tar\n')
        # Stable metadata avoids spurious COPY invalidation across fresh contexts.
        for path in stage.rglob('*'):
            os.utime(path, (0, 0), follow_symlinks=False)
        # os.rename is atomic within the destination's filesystem.
        os.rename(stage, destination)
    return source


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision', default='HEAD')
    parser.add_argument('--inputs', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(package(args.revision, args.inputs, args.output), sort_keys=True))
