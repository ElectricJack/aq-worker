#!/usr/bin/env python3
"""Offline installation smoke only. Grants no runtime/provider qualification."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import tempfile

IMAGE = Path('/opt/aq-runtime')
METADATA = Path('/usr/share/agent-q')
PROGRAMS = ('aq-worker', 'aq-session-host', 'aq-agent', 'aq-git', 'codex', 'tmux')
CODEX_COMPANIONS = ('codex-code-mode-host', 'codex-path/rg', 'codex-resources/bwrap')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def elf_interpreter(path):
    """Require Linux x86-64 ELF and detect PT_INTERP without binutils at runtime."""
    with path.open('rb') as stream:
        header = stream.read(64)
        require(len(header) == 64 and header[:6] == b'\x7fELF\x02\x01', f'{path}: not ELF64 little endian')
        require(struct.unpack_from('<H', header, 18)[0] == 62, f'{path}: not x86-64')
        offset = struct.unpack_from('<Q', header, 32)[0]
        size, count = struct.unpack_from('<HH', header, 54)
        require(size >= 56 and count <= 1024, f'{path}: invalid ELF program table')
        for index in range(count):
            stream.seek(offset + index * size)
            entry = stream.read(size)
            require(len(entry) == size, f'{path}: short ELF program header')
            if struct.unpack_from('<I', entry)[0] == 3:
                return True
    return False


def installation():
    rows = []
    for name in PROGRAMS:
        path = IMAGE / name
        metadata = path.lstat()
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_mode & 0o111
                and not metadata.st_mode & 0o022 and metadata.st_nlink == 1,
                f'{name}: unsafe installed executable')
        elf_interpreter(path)
        rows.append({'name': name, 'path': str(path), 'sha256': digest(path), 'bytes': metadata.st_size})
    return {'schema_version': 1, 'programs': rows}


def command(argv, env, expected=None, code=0):
    result = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=30)
    require(result.returncode == code, f'{argv[0]}: exit {result.returncode}: {result.stderr[:1024]}')
    if expected is not None:
        require(expected in result.stdout + result.stderr, f'{argv[0]}: missing {expected!r}')
    return result.stdout.strip()


def check_dependencies(path, env):
    if elf_interpreter(path):
        output = command(['/usr/bin/ldd', str(path)], env)
        require('not found' not in output, f'{path}: missing shared library')


def check_codex_layout(pins):
    """Refuse the version-only installation that cannot execute any tool calls."""
    for name in ('codex', *CODEX_COMPANIONS):
        path = IMAGE / name
        require(path.is_file() and not path.is_symlink(), f'{name}: missing regular Codex executable')
        metadata = path.stat()
        require(metadata.st_mode & 0o111 and not metadata.st_mode & 0o022,
                f'{name}: unsafe installed executable')
        pin = pins['inputs'][name]
        require(digest(path) == pin['sha256'] and metadata.st_size == pin['bytes'],
                f'{name}: Codex pin mismatch')
        elf_interpreter(path)


def smoke():
    pins = json.loads((METADATA / 'pins.json').read_text())
    check_codex_layout(pins)
    build = json.loads((METADATA / 'build.json').read_text())
    observed = installation()
    require(observed == json.loads((METADATA / 'images.json').read_text()), 'installed bytes changed')
    # Check Git's installed helper ELFs too; version alone misses lazy helper dependencies.
    executables = [IMAGE / name for name in (*PROGRAMS, *CODEX_COMPANIONS)]
    for path in Path('/opt/aq-git-runtime').rglob('*'):
        if path.is_file() and not path.is_symlink():
            with path.open('rb') as stream:
                if stream.read(4) == b'\x7fELF':
                    executables.append(path)
    with tempfile.TemporaryDirectory(prefix='aq-smoke-', dir=os.environ['HOME']) as home:
        env = {'PATH': os.environ['PATH'], 'HOME': home, 'CODEX_HOME': home,
               'LANG': 'C.UTF-8', 'TERM': 'xterm-256color'}
        for path in executables:
            check_dependencies(path, env)
        versions = {}
        for name in ('aq-worker', 'aq-agent'):
            versions[name] = command([str(IMAGE / name), '--version'], env,
                                     f'{name} {build["versions"][name]}')
        versions['aq-git'] = command([str(IMAGE / 'aq-git'), '--version'], env,
                                     'aq-git ' + pins['inputs']['aq-git-source.tar.gz']['version'])
        versions['codex'] = command([str(IMAGE / 'codex'), '--version'], env,
                                    'codex-cli ' + pins['inputs']['codex']['version'])
        versions['tmux'] = command([str(IMAGE / 'tmux'), '-V'], env, 'tmux 3.4')
        versions['git'] = command(['/opt/aq-git-runtime/bin/git', '--version'], env, 'git version 2.56.0')
        for subcommand in ('run-native', 'check-native'):
            command([str(IMAGE / 'aq-worker'), subcommand, '--help'], env, '--native-config')
        for subcommand in ('submit', 'status'):
            command([str(IMAGE / 'aq-agent'), 'result', subcommand, '--help'], env, subcommand)
        # Host deliberately has no public version CLI. Check loader/entrypoint and
        # retain its exact source/package version and ELF hash in the manifests.
        command([str(IMAGE / 'aq-session-host')], env, 'helper needs mode and launch directory', code=1)
        command([str(IMAGE / 'codex'), '--help'], env, 'Codex CLI')
        command([str(IMAGE / 'codex-code-mode-host'), '--help'], env, '--listen')
        command([str(IMAGE / 'codex-path/rg'), '--version'], env, 'ripgrep')
        command([str(IMAGE / 'codex-resources/bwrap'), '--version'], env, 'bubblewrap')
        command(['/usr/bin/bwrap', '--version'], env, 'bubblewrap')
        command(['/usr/bin/rg', '--version'], env, 'ripgrep')
        command(['/usr/bin/tini', '--version'], env, 'tini')
        # Start and stop a private tmux server. No provider or runtime admission.
        socket = str(Path(home) / 'tmux.sock')
        try:
            command([str(IMAGE / 'tmux'), '-S', socket, '-f', '/dev/null',
                     'new-session', '-d', '-s', 'smoke', '/bin/sleep', '30'], env)
        finally:
            subprocess.run([str(IMAGE / 'tmux'), '-S', socket, 'kill-server'], env=env,
                           capture_output=True, timeout=5)
    print(json.dumps({'schema_version': 1, 'kind': 'native_runtime_installation_smoke',
                      'source_revision': build['source_revision'], 'versions': versions,
                      'elf_dependency_checks': len(executables),
                      'authority': 'installation only; runtime, model, sandbox and end-to-end qualifications separate'}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--record', action='store_true')
    args = parser.parse_args()
    if args.record:
        check_codex_layout(json.loads((METADATA / 'pins.json').read_text()))
        (METADATA / 'images.json').write_text(json.dumps(installation(), indent=2) + '\n')
    else:
        smoke()
