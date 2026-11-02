#!/usr/bin/env python3
"""Ordinary local Git support and production run-native; no agent orchestration."""
import ctypes
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import time
import uuid

STATE = Path('/state')
CONFIG = Path('/config')
IMAGE = Path('/opt/aq-runtime')


def require(condition):
    if not condition:
        raise ValueError('interactive worker setup refused')


def exchange(request):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(5)
        client.connect(str(STATE / 'git.sock'))
        client.sendall(json.dumps(request, separators=(',', ':')).encode()+b'\n')
        with client.makefile('rb') as stream:
            frame = stream.readline(65537)
    require(len(frame) <= 65536 and frame.endswith(b'\n'))
    reply = json.loads(frame)
    require(reply.get('ok') is True and reply.get('protocol_version') == 1
            and reply.get('request_id') == request['request_id'])
    if 'operation_id' in request:
        require(reply.get('operation_id') == request['operation_id'])
    return reply['result']


def stop(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)


def run():
    require(ctypes.CDLL(None).prctl(38, 1, 0, 0, 0) == 0)
    status = Path('/proc/thread-self/status').read_text()
    require(all(int(next(line.split(':', 1)[1].strip() for line in status.splitlines()
                        if line.startswith(name+':')), 16) == 0
                for name in ['CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb']))
    programs = ['aq-worker', 'aq-session-host', 'aq-agent', 'aq-git', 'codex', 'tmux']
    require(os.statvfs(IMAGE).f_flag & os.ST_RDONLY)
    require(os.stat(IMAGE).st_dev != os.stat(STATE).st_dev)
    require(all((IMAGE/name).is_file() and os.stat(IMAGE/name).st_nlink == 1 for name in programs))
    if sys.argv[1] in ('true', 'inspect'):
        print(json.dumps({'schema_version': 1, 'kind': 'local_worker_environment_observation',
                          'no_new_privs': True, 'all_capabilities_zero': True,
                          'image_readonly': True, 'persistent_state_separate_filesystem': True,
                          'programs': programs, 'network': 'inherited_real_control_and_provider',
                          'authority': 'metadata only; native preflight still required'}))
        return 0
    for name in ['worker.toml', 'native.json']:
        metadata = (CONFIG/name).stat()
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_mode & 0o077 == 0
                and metadata.st_size <= 65536)
    if sys.argv[1] == 'check':
        return subprocess.run(
            [str(IMAGE/'aq-worker'), '--config', str(CONFIG/'worker.toml'),
             'check-native', '--native-config', str(CONFIG/'native.json')],
            stdin=subprocess.DEVNULL, close_fds=True).returncode
    metadata = (CONFIG/'source-binding.json').stat()
    require(stat.S_ISREG(metadata.st_mode) and metadata.st_mode & 0o077 == 0
            and metadata.st_size <= 65536)
    require(not (STATE/'journal/journal.json').exists())
    binding = json.loads((CONFIG/'source-binding.json').read_bytes())
    require(set(binding) == {'repository_id', 'logical_identity', 'binding_generation', 'registration_operation_id'})
    uuid.UUID(binding['repository_id']); uuid.UUID(binding['registration_operation_id'])
    require(isinstance(binding['binding_generation'], int) and binding['binding_generation'] > 0)
    git = worker = None
    try:
        git = subprocess.Popen([str(IMAGE/'aq-git'), '--state-dir', str(STATE/'git'),
                                '--listen', str(STATE/'git.sock'), '--git-executable', '/usr/bin/git', 'serve'],
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, close_fds=True)
        deadline = time.monotonic()+5
        while not (STATE/'git.sock').exists():
            require(git.poll() is None and time.monotonic() < deadline)
            time.sleep(.02)
        operation = binding['registration_operation_id']
        request = {'protocol_version': 1, 'request_id': operation, 'operation_id': operation,
                   'operation': 'repo.register', 'repository_id': binding['repository_id'],
                   'expected_binding_generation': 0,
                   'params': {'binding': {'identity': binding['logical_identity'], 'mode': 'worker',
                                         'binding_generation': binding['binding_generation'], 'object_format': 'sha1'}}}
        registered = exchange(request)
        request = {'protocol_version': 1, 'request_id': str(uuid.uuid4()), 'operation': 'repo.show',
                   'repository_id': binding['repository_id'],
                   'expected_binding_generation': binding['binding_generation'], 'params': {}}
        current = exchange(request)
        require(current == registered and current['mode'] == 'worker'
                and current['identity'] == binding['logical_identity']
                and current['binding_generation'] == binding['binding_generation'])
        worker = subprocess.Popen([str(IMAGE/'aq-worker'), '--config', str(CONFIG/'worker.toml'),
                                   'run-native', '--native-config', str(CONFIG/'native.json')], close_fds=True)
        (STATE/'worker-namespace.pid').write_text(str(worker.pid)+'\n')
        return worker.wait()
    finally:
        stop(worker)
        stop(git)


if __name__ == '__main__':
    try:
        sys.exit(run())
    except Exception:
        print('interactive worker setup or lifecycle refused; state preserved', file=sys.stderr)
        sys.exit(2)
