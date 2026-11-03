#!/usr/bin/env python3
"""Non-secret template recipe validation and fixed build assembly."""
import importlib.util
import json
from pathlib import Path
import re
from urllib.parse import urlsplit

_spec = importlib.util.spec_from_file_location('credential_scan', Path(__file__).with_name('credential_scan.py'))
scan = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scan)

FINALIZE = ('getent passwd runner >/dev/null && getent passwd agent >/dev/null && '
            'command -v tmux >/dev/null && command -v bwrap >/dev/null && '
            'command -v git >/dev/null && test -s /opt/aq/manifest.json')


def text(value, limit=128, pattern=None):
    if (not isinstance(value, str) or not 1 <= len(value.encode()) <= limit
            or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or scan.secret_shaped(value) or (pattern and not re.fullmatch(pattern, value))):
        raise ValueError('invalid or secret-shaped field')
    return value


def pinned(value):
    text(value, 512, r'[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}')
    return value


def parent_image(recipe, config, entries=()):
    parent = recipe['parent']
    if parent == config['os_image']:
        return parent
    if config.get('universal_image') and parent in {'codex-universal', config['universal_image']}:
        return config['universal_image']
    for entry in entries:
        if parent == entry['name'] + '@' + entry['version']:
            return entry['image_reference']
    raise ValueError('parent must be a pinned configured base or allowlisted toolchain')


def validate_recipe(recipe, config, entries=()):
    if not isinstance(recipe, dict) or set(recipe) - {'schema_version', 'name', 'version', 'parent', 'install_steps', 'prefetch'}:
        raise ValueError('unknown recipe fields')
    if recipe.get('schema_version') != 1:
        raise ValueError('unsupported recipe schema')
    text(recipe.get('name'), 64, r'[a-z][a-z0-9-]*')
    text(recipe.get('version'), 64, r'[A-Za-z0-9_.+-]+')
    text(recipe.get('parent'), 512)
    parent_image(recipe, config, entries)
    steps = recipe.get('install_steps')
    if not isinstance(steps, list) or len(steps) > 64:
        raise ValueError('invalid install steps')
    for step in steps:
        if not isinstance(step, list) or not 1 <= len(step) <= 32:
            raise ValueError('install step must be an argv array')
        for arg in step:
            text(arg, 4096)
        if step[0].startswith('-'):
            raise ValueError('invalid install executable')
    prefetch = recipe.get('prefetch', [])
    if not isinstance(prefetch, list) or len(prefetch) > 16:
        raise ValueError('invalid repository prefetch')
    if prefetch and config.get('registry_visibility') != 'private':
        raise ValueError('repository objects require a private target registry')
    for item in prefetch:
        if not isinstance(item, dict) or set(item) != {'repository', 'commit'}:
            raise ValueError('invalid prefetch fields')
        repo = text(item['repository'], 512)
        url = urlsplit(repo)
        if url.scheme != 'https' or not url.hostname or url.username or url.password or url.query or url.fragment:
            raise ValueError('prefetch requires a non-secret HTTPS repository URL')
        text(item['commit'], 40, r'[0-9a-f]{40}')
    return recipe


def dockerfile(recipe, base, config):
    lines = ['FROM ' + base, 'USER root']
    if base == config.get('universal_image'):
        lines += ['RUN apt-get update && apt-get install -y --no-install-recommends '
                  'ca-certificates tmux bubblewrap bash perl libcurl4 libexpat1 libssl3 zlib1g '
                  'libstdc++6 libgcc-s1 libatomic1 ncurses-term passwd && rm -rf /var/lib/apt/lists/*',
                  'RUN (getent group runner || groupadd --gid 10001 runner) && '
                  '(getent passwd runner || useradd --uid 10001 --gid runner --create-home --shell /bin/bash runner) && '
                  '(getent group agent || groupadd --gid 10002 agent) && '
                  '(getent passwd agent || useradd --uid 10002 --gid agent --create-home --shell /bin/bash agent)',
                  'COPY --from=' + config['os_image'] + ' /opt/git /opt/git',
                  'ENV PATH="/opt/aq/bin:/opt/git/bin:/usr/local/bin:/usr/bin:/bin" LANG=C.UTF-8 TERM=xterm-256color']
    lines += ['RUN ' + json.dumps(step) for step in recipe['install_steps']]
    for number, item in enumerate(recipe.get('prefetch', [])):
        dest = f'/opt/aq-repositories/{number}.git'
        lines += ['RUN ' + json.dumps(['git', 'init', '--bare', dest]),
                  'RUN ' + json.dumps(['git', '-C', dest, 'fetch', '--no-tags', '--depth=1',
                                       item['repository'], item['commit']])]
    lines += ['WORKDIR /workspace',
              'COPY --from=' + config['runtime_image'] + ' /opt/aq /opt/aq',
              'RUN ' + json.dumps(['/bin/sh', '-ec', FINALIZE])]
    return '\n'.join(lines) + '\n'


def validate_config(config):
    required = {'schema_version', 'role', 'docker_context', 'buildkit_builder', 'builder_identity',
                'os_image', 'runtime_image', 'target_repository', 'registry_visibility', 'allowlist'}
    if not isinstance(config, dict) or not required <= set(config) or set(config) - required - {'universal_image', 'docker_binary'}:
        raise ValueError('invalid build configuration fields')
    if config['schema_version'] != 1 or config['role'] != 'template-build':
        raise ValueError('an explicit template-build node is required')
    for key in ['docker_context', 'buildkit_builder', 'builder_identity']:
        text(config[key], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    for key in ['os_image', 'runtime_image', 'universal_image']:
        if key in config:
            pinned(config[key])
    if config['os_image'].split('@')[0].rsplit('/', 1)[-1] != 'aq-sandbox-os':
        raise ValueError('configured OS must be aq-sandbox-os')
    if config['runtime_image'].split('@')[0].rsplit('/', 1)[-1] != 'aq-sandbox-runtime':
        raise ValueError('configured runtime must be aq-sandbox-runtime')
    if 'universal_image' in config and config['universal_image'].split('@')[0].rsplit('/', 1)[-1] != 'codex-universal':
        raise ValueError('configured universal base must be codex-universal')
    text(config['target_repository'], 256, r'[a-z0-9][a-z0-9.:-]*/[a-z0-9][a-z0-9/_.-]*')
    if config['registry_visibility'] not in {'private', 'public'}:
        raise ValueError('registry visibility must be explicit')
    path = text(config['allowlist'], 512)
    if Path(path).is_absolute() or scan.safe_path(path) != path:
        raise ValueError('allowlist must be a relative vault path')
    if 'docker_binary' in config:
        if not Path(text(config['docker_binary'], 512)).is_absolute():
            raise ValueError('docker_binary must be absolute')
    return config


def read_json(path):
    with Path(path).open('rb') as stream:
        return scan.json_bytes(stream.read(scan.MAX_JSON + 1))


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()


def validate_entry(entry):
    fields = {'name', 'version', 'image_digest', 'image_reference', 'provenance',
              'harness_versions', 'materializations'}
    if not isinstance(entry, dict) or set(entry) != fields:
        raise ValueError('invalid allowlist entry fields')
    text(entry['name'], 64, r'[a-z][a-z0-9-]*')
    text(entry['version'], 64, r'[A-Za-z0-9_.+-]+')
    pinned(entry['image_reference'])
    if not scan.DIGEST.fullmatch(entry['image_digest']) or not entry['image_reference'].endswith('@' + entry['image_digest']):
        raise ValueError('allowlist digest mismatch')
    provenance = entry['provenance']
    expected = {'image_digest', 'recipe_digest', 'base_digest', 'runtime_bundle_digest', 'harness_versions',
                'built_at', 'builder_identity', 'finalize_policy', 'credential_scan_policy'}
    if not isinstance(provenance, dict) or set(provenance) != expected:
        raise ValueError('invalid allowlist provenance')
    if provenance['image_digest'] != entry['image_digest']:
        raise ValueError('provenance image digest mismatch')
    for key in ['recipe_digest', 'base_digest', 'runtime_bundle_digest']:
        if not scan.DIGEST.fullmatch(provenance[key]):
            raise ValueError('invalid provenance digest')
    text(provenance['builder_identity'], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(provenance['built_at'], 40, r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
    if provenance['finalize_policy'] != 'aq-finalize-v1' or provenance['credential_scan_policy'] != 'aq-credential-scan-v1':
        raise ValueError('allowlist entry has no supported admission evidence')
    versions = entry['harness_versions']
    if not isinstance(versions, dict) or not 1 <= len(versions) <= 32 or versions != provenance['harness_versions']:
        raise ValueError('inconsistent advertised harness versions')
    for name, version in versions.items():
        text(name, 128, r'[A-Za-z0-9_.+-]+')
        text(version, 128, r'[A-Za-z0-9_.+-]+')
    if not isinstance(entry['materializations'], dict) or len(canonical(entry['materializations'])) > 65536:
        raise ValueError('invalid materialization metadata')
    if scan.secret_shaped(canonical(entry)):
        raise ValueError('credential in allowlist metadata')
    return entry


def read_allowlist(path):
    if not path.exists():
        return {'schema_version': 1, 'entries': []}
    value = read_json(path)
    if not isinstance(value, dict) or set(value) != {'schema_version', 'entries'} or value['schema_version'] != 1:
        raise ValueError('unsupported allowlist schema')
    entries = value['entries']
    if not isinstance(entries, list) or len(entries) > 1024:
        raise ValueError('allowlist entry limit')
    names = set()
    for entry in entries:
        validate_entry(entry)
        key = (entry['name'], entry['version'])
        if key in names:
            raise ValueError('duplicate allowlist name/version')
        names.add(key)
    return value


def write_json(path, value):
    with path.open('xb') as output:
        output.write(canonical(value) + b'\n')


def build(recipe_path, config_path, output_path):
    import datetime
    import fcntl
    import hashlib
    import os
    import shutil
    import subprocess
    import tempfile

    cfg = validate_config(read_json(config_path))
    vault = Path(config_path).resolve().parent
    allowlist = vault / cfg['allowlist']
    # Config and vault are trusted operator inputs, never task/attempt inputs.
    entries = read_allowlist(allowlist)['entries']
    recipe = validate_recipe(read_json(recipe_path), cfg, entries)
    if any(e['name'] == recipe['name'] and e['version'] == recipe['version'] for e in entries):
        raise ValueError('recipe name/version already allowlisted; bump version')
    base = parent_image(recipe, cfg, entries)
    output = Path(output_path).absolute()
    if ',' in str(output) or '\n' in str(output):
        raise ValueError('unsupported output path')
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    docker = cfg.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker BuildKit is required on the configured build node')
    with tempfile.TemporaryDirectory(prefix='aq-template-context-') as directory:
        context = Path(directory)
        (context / 'Dockerfile').write_text(dockerfile(recipe, base, cfg))
        # Provider keys, SSH agent, attempt variables, build args/secrets and arbitrary
        # host files are never forwarded. Docker's own context/registry auth stays local.
        env = {key: os.environ[key] for key in ['PATH', 'HOME', 'XDG_CONFIG_HOME', 'DOCKER_CONFIG'] if key in os.environ}
        command = [docker, '--context', cfg['docker_context'], 'buildx', 'build',
                   '--builder', cfg['buildkit_builder'], '--platform', 'linux/amd64',
                   '--provenance=false', '--sbom=false', '--output',
                   f'type=oci,dest={output / "image.oci.tar"},compression=gzip,force-compression=true',
                   str(context)]
        try:
            subprocess.run(command, env=env, check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3600)
        except (OSError, subprocess.SubprocessError) as error:
            raise ValueError('BuildKit build/finalize failed; template not admitted') from error
    scanned = scan.scan_oci(output / 'image.oci.tar')
    versions = {item['name']: item['version'] for item in scanned['bundle_manifest']['harnesses']}
    provenance = {'image_digest': scanned['image_digest'],
                  'recipe_digest': 'sha256:' + hashlib.sha256(canonical(recipe)).hexdigest(),
                  'base_digest': base.split('@')[1],
                  'runtime_bundle_digest': cfg['runtime_image'].split('@')[1],
                  'harness_versions': versions,
                  'built_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'builder_identity': cfg['builder_identity'],
                  'finalize_policy': 'aq-finalize-v1', 'credential_scan_policy': 'aq-credential-scan-v1'}
    entry = {'name': recipe['name'], 'version': recipe['version'],
             'image_digest': scanned['image_digest'],
             'image_reference': cfg['target_repository'] + '@' + scanned['image_digest'],
             'provenance': provenance, 'harness_versions': versions, 'materializations': {}}
    validate_entry(entry)
    # Persist all reviewable artifacts before the allowlist commit point.
    write_json(output / 'recipe.json', recipe)
    write_json(output / 'provenance.json', provenance)
    write_json(output / 'bundle-manifest.json', scanned['bundle_manifest'])
    (output / 'Dockerfile').write_text(dockerfile(recipe, base, cfg))
    allowlist.parent.mkdir(parents=True, exist_ok=True)
    with allowlist.with_suffix(allowlist.suffix + '.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        current = read_allowlist(allowlist)
        # Revalidate parents and unique names under the same lock as publication.
        if parent_image(recipe, cfg, current['entries']) != base:
            raise ValueError('allowlisted parent changed during build')
        if any(e['name'] == recipe['name'] and e['version'] == recipe['version'] for e in current['entries']):
            raise ValueError('recipe version concurrently allowlisted')
        current['entries'].append(entry)
        encoded = canonical(current) + b'\n'
        if len(encoded) > scan.MAX_JSON:
            raise ValueError('allowlist size limit')
        fd, temporary = tempfile.mkstemp(prefix='.allowlist-', dir=allowlist.parent)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, allowlist)
            dirfd = os.open(allowlist.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(dirfd)
            finally:
                os.close(dirfd)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
    return entry


def main():
    import argparse
    import sys
    parser = argparse.ArgumentParser(description='Build and scan pinned sandbox templates')
    sub = parser.add_subparsers(dest='command', required=True)
    builder = sub.add_parser('build')
    builder.add_argument('recipe', type=Path)
    builder.add_argument('--build-config', type=Path, required=True)
    builder.add_argument('--output', type=Path, required=True)
    scanner = sub.add_parser('scan')
    scanner.add_argument('archive', type=Path)
    args = parser.parse_args()
    try:
        result = build(args.recipe, args.build_config, args.output) if args.command == 'build' else scan.scan_oci(args.archive)
        print(json.dumps(result, sort_keys=True))
    except (ValueError, OSError, KeyError, TypeError, RecursionError):
        # Never echo untrusted recipe fields, paths, build output or secret bytes.
        print('template refused: invalid input, build/finalize failure or credential scan failure', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
