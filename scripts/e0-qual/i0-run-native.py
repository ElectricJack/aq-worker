#!/usr/bin/env python3
"""I0 container entry: worker-side aq-git, worker-mode binding, one run-native.

Runs inside the pinned qualification worker image with the
aq-e0-qual-worker-state volume at /state. Prints public status only.
"""
import json
from pathlib import Path
import socket
import subprocess
import sys
import time
import uuid

STATE = Path('/state')
CONFIG = STATE / 'config'
IMAGE = Path('/opt/aq-runtime')
REPOSITORY = 'e0a10000-0000-4000-8000-000000000003'
IDENTITY = 'e0-qual-scratch'
GENERATION = 1


def exchange(request):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10)
        client.connect(str(STATE / 'git.sock'))
        client.sendall(json.dumps(request, separators=(',', ':')).encode() + b'\n')
        with client.makefile('rb') as stream:
            frame = stream.readline(65537)
    reply = json.loads(frame)
    if reply.get('ok') is not True or reply.get('request_id') != request['request_id']:
        raise RuntimeError(f"aq-git refused {request['operation']}: {json.dumps(reply.get('error'))[:500]}")
    return reply['result']


def stop(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def main():
    if (STATE / 'journal').exists() or (STATE / 'materials').exists():
        raise RuntimeError('state root is not fresh')
    git = worker = None
    try:
        git = subprocess.Popen([str(IMAGE / 'aq-git'), '--state-dir', str(STATE / 'git'),
                                '--listen', str(STATE / 'git.sock'), '--git-executable', '/usr/bin/git',
                                'serve'], stdin=subprocess.DEVNULL, close_fds=True)
        deadline = time.monotonic() + 10
        while not (STATE / 'git.sock').exists():
            if git.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('aq-git did not listen')
            time.sleep(0.05)
        operation = str(uuid.uuid4())
        registered = exchange({
            'protocol_version': 1, 'request_id': operation, 'operation_id': operation,
            'operation': 'repo.register', 'repository_id': REPOSITORY, 'expected_binding_generation': 0,
            'params': {'binding': {'identity': IDENTITY, 'mode': 'worker',
                                   'binding_generation': GENERATION, 'object_format': 'sha1'}}})
        current = exchange({'protocol_version': 1, 'request_id': str(uuid.uuid4()), 'operation': 'repo.show',
                            'repository_id': REPOSITORY, 'expected_binding_generation': GENERATION,
                            'params': {}})
        if current != registered or current.get('mode') != 'worker':
            raise RuntimeError('worker binding mismatch')
        print(json.dumps({'event': 'worker_binding', 'repository_id': REPOSITORY,
                          'binding_generation': GENERATION, 'mode': 'worker'}), flush=True)
        started = time.time()
        worker_config = sys.argv[1] if len(sys.argv) > 1 else str(CONFIG / 'worker.toml')
        worker = subprocess.Popen([str(IMAGE / 'aq-worker'), '--config', worker_config,
                                   'run-native', '--native-config', str(CONFIG / 'native.json')],
                                  stdin=subprocess.DEVNULL, close_fds=True)
        code = worker.wait()
        print(json.dumps({'event': 'run_native_exit', 'exit_code': code,
                          'wall_seconds': round(time.time() - started, 1)}), flush=True)
        return code
    finally:
        stop(worker)
        stop(git)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:  # public status only
        print(f'i0 run refused: {error}', file=sys.stderr, flush=True)
        sys.exit(2)
