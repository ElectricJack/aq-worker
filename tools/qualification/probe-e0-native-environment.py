#!/usr/bin/env python3
"""Measure the owner-approved E0 native worker container profile.

Runs the pinned I0 image once with the exact run flags approved for the local
I0/E0 qualification container (sharp-harbor-18 owner decision 2026-11-02,
seccomp=unconfined), except that /config and /state are fresh tmpfs instead of
host binds. Mounts no host path, forwards no credential and calls no provider.
The result is measured evidence for the environment qualification ArtifactRef:
the canonical evidence bytes are written with O_EXCL and their sha256 printed.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import uuid

IMAGE = 'sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d'
AUTHORITY = 'measured (local Docker probe of the pinned image)'
DECISION = 'sharp-harbor-18 owner decision 2026-11-02, task comment 1793676397.3807435 (owner-approved)'
# The I0 installation receipt pins, re-verified inside the container.
PINS = {
    'aq-worker': ('6785361916b0fbb630a3b5118ce71227d84b733e9db175dcf011a7a74de9b064', 20263504),
    'aq-session-host': ('9e8cdf0633061ffe5eece467ecf23849213b0cc7af0f63d11b9151373b1c24b6', 1791336),
    'aq-agent': ('37f3a5a1107bbe9314474c314b7b8a718710b0d5cceafb362842a7242de2638c', 5396720),
    'aq-git': ('9de33fd4e79d68a9cb8c72532ee03825dca3a3dbaf979bcd8770a53ef8fe9d3a', 3142096),
    'codex': ('b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8', 319346888),
    'tmux': ('48418cb9d1a947623cb3f0fc294ddf9179173231a39e83c076d5e0f4a50411b4', 1152952),
}
# Security-relevant run flags, identical to the documented native run command.
RUN_FLAGS = ['--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges=true',
             '--security-opt', 'seccomp=unconfined', '--network', 'host',
             '--tmpfs', '/tmp:mode=1777']
PROBE_MOUNTS = ['--tmpfs', '/config:mode=0700,uid=10001,gid=10001',
                '--tmpfs', '/state:mode=0700,uid=10001,gid=10001']

# Mirrors aq-session-host named_environment.rs privileges()/no_writable_mount_alias.
PROBE = r'''
import errno, hashlib, json, os, pathlib, subprocess, sys
status = dict(l.split(':', 1) for l in pathlib.Path('/proc/thread-self/status').read_text().splitlines() if ':' in l)
selected = {k: status[k].strip() for k in ('CapInh','CapPrm','CapEff','CapBnd','CapAmb','NoNewPrivs','Seccomp','Seccomp_filters')}
root = pathlib.Path('/opt/aq-runtime')
device = os.stat(root / 'codex').st_dev
name = '%d:%d' % (os.major(device), os.minor(device))
rows = [r.split() for r in pathlib.Path('/proc/thread-self/mountinfo').read_text().splitlines()]
image_rows = [r for r in rows if r[2] == name]
programs = {}
for program in ('aq-worker','aq-session-host','aq-agent','aq-git','codex','tmux'):
    path = root / program
    with path.open('rb') as stream:
        programs[program] = {'sha256': hashlib.file_digest(stream, 'sha256').hexdigest(),
                             'bytes': path.stat().st_size}
try:
    pathlib.Path('/opt/aq-runtime/.probe').write_text('x')
    root_write = 'allowed'
except OSError as error:
    root_write = errno.errorcode[error.errno]
home = pathlib.Path('/state/codex-home'); home.mkdir(mode=0o700)
work = pathlib.Path('/state/workspace'); work.mkdir(mode=0o700)
env = {'CODEX_HOME': str(home), 'HOME': str(home), 'PATH': '/usr/bin:/bin'}
version = subprocess.run([str(root / 'codex'), '--version'], capture_output=True, text=True, env=env, timeout=15)
sandbox = subprocess.run([str(root / 'codex'), 'sandbox', '--', '/bin/true'], cwd=work,
                         capture_output=True, text=True, env=env, timeout=20)
tmux = subprocess.run([str(root / 'tmux'), '-V'], capture_output=True, text=True, env=env, timeout=15)
bwrap_path = pathlib.Path('/usr/bin/bwrap')
with bwrap_path.open('rb') as stream:
    bwrap = {'path': str(bwrap_path), 'sha256': hashlib.file_digest(stream, 'sha256').hexdigest(),
             'bytes': bwrap_path.stat().st_size}
bwrap['version'] = subprocess.run([str(bwrap_path), '--version'], capture_output=True, text=True,
                                  env=env, timeout=15).stdout.strip()
print(json.dumps({'uid': os.getuid(), 'gid': os.getgid(), 'status': selected,
    'image_device': name, 'image_mount_rows': len(image_rows),
    'image_mount_rows_read_only': all('ro' in r[5].split(',') for r in image_rows),
    'opt_write': root_write, 'programs': programs, 'os_bwrap': bwrap,
    'codex_version': version.stdout.strip(), 'tmux_version': tmux.stdout.strip(),
    'codex_sandbox_true_exit': sandbox.returncode}, sort_keys=True))
'''
BASELINE = ("print(next(l.split(':')[1].strip() for l in open('/proc/self/status')"
            " if l.startswith('Seccomp_filters:')))")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False,
                      allow_nan=False).encode()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify(facts):
    """Return the failed checks; the evidence is recorded either way."""
    status = facts['status']
    checks = {
        'uid_10001': facts['uid'] == facts['gid'] == 10001,
        'capability_sets_empty': all(int(status[k], 16) == 0 for k in
                                     ('CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb')),
        'no_new_privs': status['NoNewPrivs'] == '1',
        # Docker Desktop keeps one host baseline filter even under --privileged;
        # unconfined must remove exactly Docker's own profile filter.
        'docker_seccomp_profile_removed':
            int(status['Seccomp_filters']) == facts['default_profile_seccomp_filters'] - 1,
        'image_device_read_only': facts['image_mount_rows'] > 0 and facts['image_mount_rows_read_only'],
        'image_write_refused': facts['opt_write'] == 'EROFS',
        'program_pins': {k: (v['sha256'], v['bytes']) for k, v in facts['programs'].items()} == PINS,
        'codex_0_166_0': facts['codex_version'] == 'codex-cli 0.166.0',
        'tmux_3_4': facts['tmux_version'] == 'tmux 3.4',
        # Recorded measured pin for the environment body; the OS image provides it.
        'os_bwrap_present': bool(re.fullmatch(r'[0-9a-f]{64}', facts['os_bwrap']['sha256']))
            and facts['os_bwrap']['version'].startswith('bubblewrap '),
        'codex_sandbox_namespaces': facts['codex_sandbox_true_exit'] == 0,
    }
    return checks, sorted(k for k, ok in checks.items() if not ok)


def probe(image):
    require(re.fullmatch(r'sha256:[0-9a-f]{64}', image), 'immutable local image ID required')
    inspect = subprocess.run(['docker', 'image', 'inspect', '--format', '{{json .}}', image],
                             capture_output=True, text=True, timeout=30)
    require(inspect.returncode == 0, 'image not installed locally')
    metadata = json.loads(inspect.stdout)
    require(metadata['Id'] == image, 'image identity mismatch')
    name = 'aq-e0-env-probe-' + uuid.uuid4().hex[:12]
    command = (['docker', 'run', '--rm', '--pull=never', '--name', name] + RUN_FLAGS + PROBE_MOUNTS
               + ['--entrypoint', '/usr/bin/python3', image, '-c', PROBE])
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=90)
    except subprocess.TimeoutExpired:
        subprocess.run(['docker', 'rm', '-f', name], capture_output=True, timeout=30)
        raise
    require(result.returncode == 0, 'probe container failed: ' + result.stderr[-400:])
    facts = json.loads(result.stdout)
    # Baseline: the same image under Docker's default seccomp profile.
    baseline = subprocess.run(['docker', 'run', '--rm', '--pull=never', '--network', 'none',
                               '--entrypoint', '/usr/bin/python3', image, '-c', BASELINE],
                              capture_output=True, text=True, timeout=60)
    require(baseline.returncode == 0, 'baseline container failed')
    facts['default_profile_seccomp_filters'] = int(baseline.stdout)
    checks, failed = verify(facts)
    return {'schema_version': 1, 'kind': 'aq.e0-native-environment-probe', 'authority': AUTHORITY,
            'decision': DECISION, 'image_id': image,
            'platform': metadata['Os'] + '/' + metadata['Architecture'],
            'run_flags': RUN_FLAGS,
            'probe_substitution': '/config and /state were fresh tmpfs, not the host binds',
            'facts': facts, 'checks': checks, 'passed': not failed, 'failed': failed}


def write_new(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
    with os.fdopen(fd, 'wb') as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default=IMAGE)
    parser.add_argument('--output', required=True, help='new file for canonical evidence bytes')
    args = parser.parse_args()
    report = probe(args.image)
    data = canonical(report)
    write_new(args.output, data)
    print(json.dumps({'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data),
                      'passed': report['passed'], 'failed': report['failed']}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
