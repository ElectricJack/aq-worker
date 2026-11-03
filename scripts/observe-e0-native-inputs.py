#!/usr/bin/env python3
"""Record the bounded MS0 owner attestation; never generate admission authority."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
import tomllib

AUTHORITY = 'owner-attested (recorded by agent for the owner)'
AUTHORIZATION = 'sharp-harbor-18 owner decision 2026-11-02, task comment 1793671179.038809'
MISSING = {
    'model': ['provider', 'model', 'native_reasoning',
              'reasoning_configuration_reference', 'max_output_tokens'],
    'context': ['maximum_tokens_per_utf8_byte', 'fixed_overhead_tokens',
                'counting_evidence', 'capacity_kind', 'capacity_tokens',
                'capacity_evidence', 'observed_at_ms', 'expires_at_ms'],
    'qualification': ['runtime', 'runtime_qualification', 'adapter', 'renderer',
                      'environment', 'harness_qualification', 'scoped_presence',
                      'installation_and_result_client_manifests'],
    'credential': ['profile', 'revision', 'account_reference', 'endpoint_reference',
                   'allowed_node_ids', 'private_secret_file', 'material_delivery'],
    'publication': ['project_policy', 'replacement_catalog', 'catalog_activation',
                    'project_binding_update'],
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON field')
        result[key] = value
    return result


def load_json(data):
    # Do not propagate JSONDecodeError: its diagnostic object retains the input.
    try:
        value = json.loads(data, object_pairs_hook=unique_object)
    except (ValueError, UnicodeError):
        raise ValueError('invalid JSON input') from None
    require(isinstance(value, (dict, list)), 'JSON object or array required')
    return value


def read_file(path, private=False):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as source:
        before = os.fstat(source.fileno())
        require(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= 1048576,
                'input must be a bounded regular file')
        if private:
            require(before.st_uid == os.geteuid()
                    and stat.S_IMODE(before.st_mode) == 0o600,
                    'auth input must be owned by the current user with mode 0600')
        data = source.read(1048577)
        after = os.fstat(source.fileno())
        named = path.lstat()
        identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
        require(identity(before) == identity(after) == identity(named)
                and len(data) == before.st_size, 'input changed while reading')
    return data


def auth_observation(path):
    auth = load_json(read_file(path, private=True))
    require(isinstance(auth, dict) and auth.get('auth_mode') == 'chatgpt'
            and isinstance(auth.get('tokens'), dict) and bool(auth['tokens']),
            'expected a ChatGPT file login record')
    # Never return token fields, account IDs, private fingerprints or auth bytes.
    return {'path': str(path), 'auth_mode': 'chatgpt', 'file_store_record_present': True,
            'owner_only_regular_file': True, 'permissions': '0600',
            'authentication_validity': 'not tested', 'lease_input_created': False}


def run(argv, timeout=30):
    result = subprocess.run(argv, capture_output=True, timeout=timeout)
    require(result.returncode == 0 and len(result.stdout) <= 1048576,
            'local observation command failed; no child output disclosed')
    return result.stdout


# No credentials, mounts, network, sandbox relaxation, or provider invocation.
IMAGE_PROBE = r'''
import hashlib, json, os, pathlib, subprocess
root = pathlib.Path('/opt/aq-runtime')
programs = {}
for name in ('aq-worker', 'aq-session-host', 'aq-agent', 'aq-git', 'codex', 'tmux'):
    path = root / name
    with path.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    programs[name] = {'sha256': digest, 'bytes': path.stat().st_size}
result = subprocess.run([str(root / 'codex'), '--version'], capture_output=True,
    timeout=15, env={'HOME':'/nonexistent', 'CODEX_HOME':'/nonexistent', 'PATH':'/usr/bin:/bin'})
assert result.returncode == 0 and result.stdout.strip() == b'codex-cli 0.166.0'
build_path = pathlib.Path('/usr/share/agent-q/build.json')
build = json.loads(build_path.read_text()) if build_path.exists() else {}
print(json.dumps({'source_revision':build.get('source_revision'), 'programs':programs,
                  'codex_version':'0.166.0'}))
'''


def image_observation(image):
    require(re.fullmatch(r'sha256:[0-9a-f]{64}', image), 'immutable local image ID required')
    metadata = load_json(run(['docker', 'image', 'inspect', image]))
    require(len(metadata) == 1 and metadata[0]['Id'] == image, 'image identity mismatch')
    facts = load_json(run([
        'docker', 'run', '--rm', '--pull=never', '--network=none', '--read-only',
        '--cap-drop=ALL', '--security-opt=no-new-privileges', '--user=10001:10001',
        '--entrypoint=/usr/bin/python3', image, '-c', IMAGE_PROBE], timeout=60))
    labels = metadata[0].get('Config', {}).get('Labels', {}) or {}
    source_label = labels.get('org.opencontainers.image.revision', '')
    require(re.fullmatch(r'[0-9a-f]{40}', source_label), 'missing image source revision label')
    build_revision = facts.pop('source_revision')
    require(build_revision is None or build_revision == source_label,
            'image source label differs from build metadata')
    return {'image_id': image, 'platform': metadata[0]['Os'] + '/' + metadata[0]['Architecture'],
            **facts, 'source_revision_label': source_label,
            'build_metadata_present': build_revision is not None,
            'scope': 'installed bytes and version only; source label is not build provenance; no runtime qualification'}


def e0_observation(root):
    evidence = load_json(read_file(root / 'seed-evidence.json'))
    seed = load_json(read_file(root / 'state/seed.json'))
    exchanges = load_json(read_file(root / 'source-base-exchanges.json'))
    require(seed == evidence['seed'], 'seed receipt mismatch')
    require(seed['designation'] == {
        'base_branch': 'main', 'credential_ref': 'none', 'local_fixture': False,
        'url': 'https://github.com/ElectricJack/aq-scratch.git'}, 'owner designation mismatch')
    base = evidence['source_base']
    retained = base['retention']
    require(retained['id'] == base['retention_handle'] and retained['state'] == 'active'
            and retained['repository_id'] == seed['repository_id']
            and retained['binding_generation'] == evidence['repository']['binding_generation']
            and any(r['oid'] == base['base']['hex'] for r in retained['refs']),
            'retained source receipt mismatch')
    inspected = [item for item in exchanges if item['request']['operation'] == 'retention.inspect']
    require(len(inspected) == 1 and inspected[0]['response'].get('ok') is True
            and inspected[0]['response']['result'] == retained, 'retention exchange mismatch')
    config = tomllib.loads(read_file(root / 'state/vault/config.toml').decode())
    catalog = load_json(read_file(root / 'source/scripts/dev-catalog.json'))
    # These hashes identify public saved receipts, never protocol ArtifactRefs.
    receipts = {name: hashlib.sha256(read_file(root / name)).hexdigest() for name in
                ('seed-evidence.json', 'source-base-exchanges.json', 'state/seed.json')}
    return {'source_revision': evidence['source_revision'], 'seed': seed,
            'base': base['base'], 'retention_handle': base['retention_handle'],
            'receipt_sha256': receipts,
            'verification_scope': 'saved E0 receipts only; live graph and retention not rechecked',
            'saved_graph_task_verified': evidence['graph_task_verified'],
            'configuration_sections_present': {name: name in config for name in
                                               ('intelligence', 'profiled_execution')},
            'saved_catalog_candidates': [
                {'provider': c['provider'], 'model': c['model'],
                 'reasoning_configuration_reference': c['reasoning_configuration_reference']}
                for profile in catalog['catalog']['intelligence'] for c in profile['candidates']]}


def collect(stack_root, auth_file, codex_binary, image):
    version = run([str(codex_binary), '--version']).strip()
    require(version == b'codex-cli 0.166.0', 'expected installed Codex CLI 0.166.0')
    return {'schema_version': 1, 'authority': AUTHORITY, 'authorization': AUTHORIZATION,
            'recorded_at_ms': time.time_ns() // 1000000,
            'status': 'blocked_missing_owner_qualification',
            'native_configuration_emitted': False,
            'records': [
                {'authority': AUTHORITY, 'kind': 'host_cli', 'harness': 'codex',
                 'cli_version': '0.166.0', 'binary': str(codex_binary)},
                {'authority': AUTHORITY, 'kind': 'owner_login', **auth_observation(auth_file)},
                {'authority': AUTHORITY, 'kind': 'runtime_image', **image_observation(image)},
                {'authority': AUTHORITY, 'kind': 'e0_context_and_retention', **e0_observation(stack_root)}],
            'missing_owner_inputs': MISSING,
            'limits': ['owner attestation is not measured qualification evidence',
                       'no provider authentication, model capacity or token counting tested',
                       'no native admission, material lease, registration or polling performed',
                       'no E0 source, live configuration, catalog or private credential modified']}


def write_report(path, report):
    # A fresh private file; never overwrite earlier evidence or follow a symlink.
    data = (json.dumps(report, indent=2, sort_keys=True) + '\n').encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stack-root', required=True, type=Path)
    parser.add_argument('--auth-file', required=True, type=Path)
    parser.add_argument('--codex-binary', required=True, type=Path)
    parser.add_argument('--runtime-image', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    try:
        report = collect(args.stack_root, args.auth_file, args.codex_binary, args.runtime_image)
        write_report(args.output, report)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print('Observation failed; inputs/output were refused. No private or child output disclosed.', file=sys.stderr)
        return 1
    print('Recorded bounded owner attestation; real native qualification remains blocked.')
    return 0


if __name__ == '__main__':
    sys.exit(main())
