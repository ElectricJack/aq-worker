#!/usr/bin/env python3
"""Credential-free, opt-in qualification of the pinned Codex Docker profile.

Requires a Linux/amd64 Docker daemon and the already installed I0 image. Does
not build/pull images, mount host paths, forward credentials or call a provider.
The output directory must be new. Each disposable container has a 30s timeout.
"""
import argparse
import copy
import hashlib
import json
import pathlib
import subprocess
import tempfile
import uuid

ROOT = pathlib.Path(__file__).resolve().parents[2]
IMAGE = "sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d"
PROFILE = ROOT / "tools/qualification/seccomp/codex-bwrap-amd64.json"
BASE = ROOT / "vendor/docker-seccomp/default.json"

# Executed inside the container; all state is fresh tmpfs. The canary also
# checks a directory writable to the outer UID but outside Codex's workspace.
CANARY = r'''
import errno, os, pathlib
assert os.getuid() == os.getgid() == 10001
status = dict(line.split(':', 1) for line in pathlib.Path('/proc/self/status').read_text().splitlines() if ':' in line)
assert all(int(status[key].strip(), 16) == 0 for key in ['CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb'])
assert status['NoNewPrivs'].strip() == '1'
assert status['Seccomp'].strip() == '2'
p = pathlib.Path('canary.txt')
p.write_text('workspace write/read ok\n')
assert p.read_text() == 'workspace write/read ok\n'
for name in ['/state/outside/forbidden.txt', '/etc/aq-codex-probe']:
    try:
        pathlib.Path(name).write_text('unexpected write')
    except OSError as error:
        assert error.errno in [errno.EACCES, errno.EPERM, errno.EROFS], (name, error)
    else:
        raise AssertionError('unexpected write: ' + name)
print('PASS workspace read/write; outside/root writes refused; UID/caps/NNP/seccomp retained')
'''

SETUP = r'''
import ctypes, errno, hashlib, json, os, pathlib, platform, subprocess, sys
home = pathlib.Path('/state/codex'); home.mkdir(mode=0o700)
work = pathlib.Path('/state/workspace'); work.mkdir(mode=0o700)
outside = pathlib.Path('/state/outside'); outside.mkdir(mode=0o700)
assert home.stat().st_mode & 0o777 == 0o700
assert os.getuid() == os.getgid() == 10001
os.environ['CODEX_HOME'] = str(home)
os.environ['HOME'] = '/home/aq'
if sys.argv[1] == 'metadata':
    versions = {}
    for executable in ['/opt/aq-runtime/codex', '/usr/bin/bwrap']:
        completed = subprocess.run([executable, '--version'], capture_output=True, text=True, check=True)
        versions[executable] = {'version': completed.stdout.strip(), 'sha256': hashlib.sha256(pathlib.Path(executable).read_bytes()).hexdigest()}
    assert versions['/opt/aq-runtime/codex']['version'] == 'codex-cli 0.166.0', versions
    try:
        pathlib.Path('/etc/aq-codex-probe').write_text('unexpected write')
    except OSError as error:
        assert error.errno == errno.EROFS, error
    else:
        raise AssertionError('root filesystem is writable')
    status = dict(line.split(':', 1) for line in pathlib.Path('/proc/self/status').read_text().splitlines() if ':' in line)
    selected = {k: status[k].strip() for k in ['CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb', 'NoNewPrivs', 'Seccomp']}
    assert all(int(selected[k], 16) == 0 for k in ['CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb'])
    assert selected['NoNewPrivs'] == '1' and selected['Seccomp'] == '2'
    print(json.dumps({'versions': versions, 'kernel': platform.release(), 'status': selected, 'home_mode': oct(home.stat().st_mode & 0o777), 'root_read_only': True}, sort_keys=True))
    sys.exit(0)
elif sys.argv[1] == 'canary':
    # Confirm this path is writable before Codex applies its own policy.
    (outside / 'outer.txt').write_text('outer UID can write here')
    result = subprocess.run(['/opt/aq-runtime/codex', 'sandbox', '-c', 'sandbox_mode="workspace-write"', '--', '/usr/bin/python3', '-c', sys.argv[2]], cwd=work)
elif sys.argv[1] == 'denials':
    # x86_64 syscall numbers; run outside Codex to inspect the Docker layer.
    libc = ctypes.CDLL(None, use_errno=True)
    for name, number, args, expected in [
        ('unshare-zero', 272, [0], errno.EPERM),
        ('unshare-net', 272, [0x40000000], errno.EPERM),
        ('clone3-fallback', 435, [0, 0], errno.ENOSYS),
        ('keyctl', 250, [0, 0, 0, 0, 0], errno.EPERM),
        ('bpf', 321, [0, 0, 0], errno.EPERM),
        ('io_uring_setup', 425, [0, 0], errno.EPERM),
    ]:
        ctypes.set_errno(0)
        actual = libc.syscall(ctypes.c_long(number), *[ctypes.c_long(a) for a in args])
        observed = ctypes.get_errno()
        assert actual == -1 and observed == expected, (name, actual, observed)
        print('PASS', name, errno.errorcode[observed])
    sys.exit(0)
else:
    result = subprocess.run(['/opt/aq-runtime/codex', 'sandbox', '--', '/bin/true'], cwd=work)
sys.exit(result.returncode)
'''


def execute(command, timeout=30):
    result = subprocess.run(command, text=True, capture_output=True, timeout=timeout)
    return {"command": command, "exit": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--image', default=IMAGE, help='locally installed image ID to requalify')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    report = {"image_requested": args.image, "passed": False, "results": []}

    def save():
        (args.output / 'qualification.json').write_text(json.dumps(report, indent=2) + '\n')

    try:
        version = execute(['docker', 'version', '--format', '{{json .}}'])
        report['docker_version'] = version
        assert version['exit'] == 0, version
        server = json.loads(version['stdout'])['Server']
        assert (server['Os'], server['Arch']) == ('linux', 'amd64'), server
        image = execute(['docker', 'image', 'inspect', args.image, '--format',
                         '{{json .Id}} {{json .Architecture}} {{json .Config.User}}'])
        report['image_inspect'] = image
        assert image['exit'] == 0, image
        assert image['stdout'].strip().endswith('"amd64" "10001:10001"'), image
        report['profiles'] = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                              for p in (BASE, PROFILE)}
        options = ['docker', 'run', '--rm', '--init', '--read-only', '--user', '10001:10001',
                   '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges=true',
                   '--network', 'none', '--pids-limit', '128', '--memory', '512m',
                   '--cpus', '1', '--tmpfs', '/state:rw,nosuid,nodev,mode=1777,size=64m',
                   '--tmpfs', '/tmp:rw,nosuid,nodev,mode=1777,size=64m']

        def run(name, policy, mode='true', expected=0, error_text=None):
            container = 'aq-codex-probe-' + uuid.uuid4().hex
            command = options + ['--name', container]
            if policy:
                command += ['--security-opt', 'seccomp=' + str(policy)]
            command += ['--entrypoint', 'python3', args.image, '-c', SETUP, mode, CANARY]
            try:
                result = execute(command)
            except subprocess.TimeoutExpired:
                # Stop only this probe's uniquely named, credential-free container.
                cleanup = execute(['docker', 'rm', '-f', container])
                report['results'].append({'name': name, 'command': command,
                                          'timeout_seconds': 30, 'cleanup': cleanup})
                save()
                raise
            result.update(name=name, expected_exit=expected)
            if policy and policy != 'unconfined':
                result['profile_sha256'] = hashlib.sha256(pathlib.Path(policy).read_bytes()).hexdigest()
            report['results'].append(result)
            save()
            assert result['exit'] == expected, result
            if error_text:
                assert error_text in result['stderr'], result
            print(f"PASS {name}: exit {result['exit']}", flush=True)

        run('docker-default', None, expected=1, error_text='No permissions to create new namespace')
        run('unconfined-control', 'unconfined')
        run('pinned-upstream-default', BASE, expected=1, error_text='No permissions to create new namespace')
        run('metadata', PROFILE, mode='metadata')
        run('narrow-profile', PROFILE)
        run('workspace-canary', PROFILE, mode='canary')
        run('retained-denials', PROFILE, mode='denials')
        # Each necessity check removes exactly one of the five added allowances.
        with tempfile.TemporaryDirectory(prefix='aq-codex-seccomp-') as temporary:
            for call, message in [('clone', 'No permissions to create new namespace'),
                                  ('unshare', 'unshare user ns'), ('mount', 'Failed to make / slave'),
                                  ('umount2', 'unmount old root'), ('pivot_root', 'pivot_root')]:
                profile = copy.deepcopy(json.loads(PROFILE.read_bytes()))
                for rule in profile['syscalls'][-3:]:
                    if call in rule['names']:
                        rule['names'].remove(call)
                profile['syscalls'] = [rule for rule in profile['syscalls'] if rule['names']]
                path = pathlib.Path(temporary) / (call + '.json')
                path.write_text(json.dumps(profile))
                run('without-' + call, path, expected=1, error_text=message)
        report['passed'] = True
    finally:
        save()


if __name__ == '__main__':
    main()
