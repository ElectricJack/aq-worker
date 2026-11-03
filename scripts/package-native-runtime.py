#!/usr/bin/env python3
"""Create a new, credential-free Docker context from a committed worker tree."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = Path('packaging/native-runtime')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def checked_copy(source, destination, pin):
    # Copy through an opened regular file, then verify the exact copied bytes.
    with source.open('rb') as stream:
        if source.is_symlink() or not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError(f'not a regular input: {source.name}')
        with destination.open('xb') as output:
            shutil.copyfileobj(stream, output)
    if digest(destination) != pin['sha256']:
        raise ValueError(f'input checksum mismatch: {source.name}')
    if 'bytes' in pin and destination.stat().st_size != pin['bytes']:
        raise ValueError(f'input size mismatch: {source.name}')


def package(revision, inputs, destination):
    revision = subprocess.check_output(
        ['git', 'rev-parse', '--verify', revision + '^{commit}'], cwd=ROOT, text=True).strip()
    if destination.exists():
        raise ValueError('destination already exists; use a fresh context directory')
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Never walk the worktree, operator home, .aq, .git, or a credential directory.
    with tempfile.TemporaryDirectory(prefix='aq-runtime-') as temporary:
        context = Path(temporary)
        archive = context / 'worker-source.tar'
        subprocess.run(['git', 'archive', '--format=tar', '--output', str(archive), revision,
                        'Cargo.toml', 'Cargo.lock', 'crates', 'vendor', str(PACKAGE)],
                       cwd=ROOT, check=True)
        with tarfile.open(archive) as source:
            source.extractall(context / 'source', filter='data')
        source = context / 'source'
        packaging = source / PACKAGE
        pins = json.loads((packaging / 'pins.json').read_text())
        for name, pin in pins['inputs'].items():
            checked_copy(inputs / name, context / name, pin)
        for name in ('Dockerfile', 'smoke.py', 'pins.json'):
            shutil.copyfile(packaging / name, context / name)
        versions = {}
        for binary, crate in [('aq-worker', 'aq-worker'), ('aq-agent', 'aq-agent-client'),
                              ('aq-session-host', 'aq-session-host')]:
            versions[binary] = tomllib.loads((source / 'crates' / crate / 'Cargo.toml').read_text())['package']['version']
        metadata = {'schema_version': 1, 'source_revision': revision,
                    'worker_archive_sha256': digest(archive), 'versions': versions}
        (context / 'build.json').write_text(json.dumps(metadata, indent=2) + '\n')
        checks = [f'{digest(context / name)}  {name}\n' for name in
                  ['worker-source.tar', 'build.json', 'pins.json', 'smoke.py', *pins['inputs']]]
        (context / 'SHA256SUMS').write_text(''.join(checks))
        # The extracted copy is only used for metadata; send the archive alone.
        shutil.rmtree(source)
        shutil.copytree(context, destination)
    return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision', default='HEAD', help='committed worker revision (default HEAD)')
    parser.add_argument('--inputs', required=True, type=Path, help='directory of the four pinned public inputs')
    parser.add_argument('--output', required=True, type=Path, help='new Docker context directory')
    args = parser.parse_args()
    print(json.dumps(package(args.revision, args.inputs, args.output), indent=2))


if __name__ == '__main__':
    main()
