#!/usr/bin/env python3
"""Private test bootstrap for ordinary RunNative. No admission/host orchestration."""
import array
import hashlib
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import time

STATE = Path('/var/lib/aq-qualification/state')
IMAGE = Path('/opt/aq-qualification')
MINIMUM_FREE = 11005853696
MAX_PACKET = 131072


def require(condition, message):
    if not condition:
        raise ValueError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate fixture field')
        result[key] = value
    return result


def receive(control):
    data, ancillary, flags, _ = control.recvmsg(MAX_PACKET + 1, 0)
    require(data and len(data) <= MAX_PACKET and not ancillary
            and not flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC), 'bounded fixture packet')
    value = json.loads(data, object_pairs_hook=unique_object)
    require(isinstance(value, dict), 'closed fixture packet')
    return value


def private_file(path, value):
    require(isinstance(value, bytes) and 0 < len(value) <= 65536, 'bounded private file')
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'wb') as output:
        output.write(value)
        output.flush()
        os.fsync(output.fileno())


def pin(path, expected, maximum):
    metadata = path.lstat()
    require(stat.S_ISREG(metadata.st_mode) and 0 < metadata.st_size <= maximum
            and metadata.st_mode & 0o111 and os.statvfs(path).f_flag & os.ST_RDONLY, 'readonly image identity')
    digest = hashlib.sha256()
    with path.open('rb') as image:
        for chunk in iter(lambda: image.read(1024 * 1024), b''):
            digest.update(chunk)
    require(digest.hexdigest() == expected, 'image digest mismatch')


def configure(value, control_origin):
    native = value.get('native_config', {})
    kind = native.get('executor', {}).get('kind')
    required = {'schema_version', 'operation', 'worker_config', 'native_config',
                'installation_manifest', 'inventory_definition'}
    if kind == 'codex-0.166-trusted-node-named-readonly-result-local-v1':
        required |= {'client_manifest', 'composite_definition'}
    else:
        require(kind == 'codex-0.166-trusted-node-named-readonly-v1', 'explicit named runner profile')
    replay = value.get('fixture_replay_lost_b1_ack', False)
    if 'fixture_replay_lost_b1_ack' in value:
        require(replay is True and kind == 'codex-0.166-trusted-node-named-readonly-result-local-v1',
                'explicit result-local replay fixture required')
        required.add('fixture_replay_lost_b1_ack')
    require(set(value) == required and value['schema_version'] == 1
            and value['operation'] == 'configure', 'closed configure packet')
    require(isinstance(native, dict) and native['state_dir'] == str(STATE)
            and native['git_root'] == str(STATE / 'git')
            and native['git_socket'] == str(STATE / 'git.sock')
            and native['adapter_manifest'] == str(STATE / 'config/adapter.json')
            and native['inventory_definition'] == str(STATE / 'config/inventory.json'), 'fixture state paths')
    require(native['executor']['settings']['host_root'] == str(STATE / 'host'), 'fixture host scope')
    disk = os.statvfs(STATE)
    require(0 < native['capacity']['disk_bytes'] <= disk.f_blocks * disk.f_frsize, 'truthful private disk capacity')
    # Trusted parent serializes the actual WorkerConfig with this origin. This
    # is an early shape check, not a TOML parser or an authority decision; the
    # ordinary CLI still validates its closed configuration before admission.
    require(isinstance(value['worker_config'], str) and control_origin in value['worker_config'], 'private control origin marker')
    files = [('native_config', 'native.json'), ('installation_manifest', 'adapter.json'),
             ('inventory_definition', 'inventory.json')]
    if kind == 'codex-0.166-trusted-node-named-readonly-result-local-v1':
        require(native['executor']['client_manifest'] == str(STATE / 'config/client.json')
                and native['executor']['composite_definition'] == str(STATE / 'config/composite.json'),
                'fixed result installation input files')
        files += [('client_manifest', 'client.json'), ('composite_definition', 'composite.json')]
    # Validate every bounded payload before creating any state. The ordinary
    # runner remains responsible for semantic/authority validation.
    encoded = [('worker.toml', value['worker_config'].encode())]
    encoded += [(name, json.dumps(value[key], sort_keys=True, ensure_ascii=False,
                                 separators=(',', ':'), allow_nan=False).encode())
                for key, name in files]
    for _, body in encoded:
        require(0 < len(body) <= 65536, 'bounded private file')
    (STATE / 'config').mkdir(mode=0o700)
    (STATE / 'host').mkdir(mode=0o700)
    for name, body in encoded:
        private_file(STATE / 'config' / name, body)
    return replay


def stop(child, grace):
    if child is None or child.poll() is not None:
        return True
    try:
        try:
            child.terminate()
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            try:
                child.kill()
            except ProcessLookupError:
                pass
            child.wait(timeout=3)
        return True
    except Exception:
        # No unbounded wait and no claim that this is an AQ Stopped proof.
        print('native runner fixture process cleanup unknown', file=sys.stderr, flush=True)
        return False


def run():
    stage = 'premises'
    control = None
    git = None
    worker = None
    listeners = []
    root_fd = None
    try:
        require(os.environ.get('AQ_NATIVE_FIXTURE_CONFINED') == 'v1', 'fixture premises absent')
        require(STATE.parent.is_dir() and not STATE.exists(), 'fresh fixture state required')
        pin(IMAGE / 'aq-worker', os.environ['AQ_NATIVE_WORKER_SHA256'], 256 * 1024 * 1024)
        pin(IMAGE / 'aq-git', os.environ['AQ_NATIVE_GIT_SHA256'], 64 * 1024 * 1024)
        if 'AQ_NATIVE_AGENT_CLIENT_SHA256' in os.environ:
            pin(IMAGE / 'aq-agent', os.environ['AQ_NATIVE_AGENT_CLIENT_SHA256'], 64 * 1024 * 1024)
        # Actual Codex/helper/tmux hashing and the live named environment owner
        # remain in the ordinary installed factory, before the offer/grant.
        STATE.mkdir(mode=0o700)
        disk = os.statvfs(STATE)
        free = disk.f_bavail * disk.f_frsize
        require(free > MINIMUM_FREE, 'unchanged Git minimum unavailable in private state')
        stage = 'listeners'
        for _ in range(2):
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            listener.set_inheritable(False)
            listener.bind(('127.0.0.1', 0))
            listener.listen(64)
            listeners.append(listener)
        control_origin = 'http://127.0.0.1:%d/' % listeners[0].getsockname()[1]
        provider_origin = 'http://127.0.0.1:%d/' % listeners[1].getsockname()[1]
        root_fd = os.open(STATE, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW)
        control = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        control.settimeout(30)
        control.connect(str(IMAGE / 'fixture-control.sock'))
        packet = {'schema_version': 1, 'phase': 'listeners', 'control_origin': control_origin,
                  'provider_origin': provider_origin, 'state_dir': str(STATE), 'state_free_bytes': free,
                  'private_pid_namespace': os.readlink('/proc/thread-self/ns/pid')}
        encoded = json.dumps(packet, separators=(',', ':')).encode()
        rights = array.array('i', [listeners[0].fileno(), listeners[1].fileno(), root_fd])
        require(control.sendmsg([encoded], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, rights)]) == len(encoded), 'exact listener transfer')
        for listener in listeners:
            listener.close()
        listeners = []
        os.close(root_fd)
        root_fd = None
        stage = 'configure'
        replay = configure(receive(control), control_origin)
        # The consuming outer receiver closes/unlinks its sole listener before
        # sending this one configuration. Close this connection before spawning
        # worker; no provisioning descriptor is inherited. The parent keeps
        # only read-side state evidence and private-net listener ownership.
        control.close()
        control = None
        stage = 'git_start'
        git = subprocess.Popen([str(IMAGE / 'aq-git'), '--state-dir', str(STATE / 'git'),
                                '--git-executable', '/usr/bin/git', '--listen', str(STATE / 'git.sock'), 'serve'],
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, close_fds=True)
        until = time.monotonic() + 2
        while not (STATE / 'git.sock').exists():
            require(git.poll() is None and time.monotonic() < until, 'bounded actual Git startup')
            time.sleep(.01)
        require(os.statvfs(STATE).f_bavail * os.statvfs(STATE).f_frsize > MINIMUM_FREE,
                'unchanged Git minimum after provisioning')
        stage = 'ordinary_runner'
        arguments = [str(IMAGE / 'aq-worker'), '--config', str(STATE / 'config/worker.toml'),
                     'run-native', '--native-config', str(STATE / 'config/native.json'),
                     '--development-loopback', '--development-provider', provider_origin,
                     '--fixture-preflight-gate']
        if replay:
            arguments.append('--fixture-replay-lost-b1-ack')
        worker = subprocess.Popen(arguments, close_fds=True)
        result = worker.wait(timeout=120)
        require(stop(git, 3), 'Git process cleanup unknown')
        return result
    except Exception:
        # Fixed stage only; never stringify config, auth, body or arbitrary errors.
        print('native runner fixture bootstrap refused at ' + stage, file=sys.stderr, flush=True)
        return 2
    finally:
        if control is not None:
            control.close()
        for listener in listeners:
            listener.close()
        if root_fd is not None:
            os.close(root_fd)
        # Cleanup may finish after admission ended. This never claims the
        # worker's durable ECHILD/ACK or releases AQ task/workspace authority.
        worker_reaped = stop(worker, 10)
        git_reaped = stop(git, 3)
        if not worker_reaped or not git_reaped:
            return 2


if __name__ == '__main__':
    sys.exit(run())
