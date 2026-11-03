"""Explicit offline acceptance probe; TEMPLATE_ENV_IMAGE names a local Rust template."""
import json
import os
from pathlib import Path
import subprocess
import uuid


ROOT = Path(__file__).resolve().parents[3]
PROBE = r'''
import gzip, http.server, json, os, pathlib, subprocess, sys, threading

home = pathlib.Path('/state/home'); home.mkdir(mode=0o700)
work = pathlib.Path('/state/work'); work.mkdir(mode=0o700)
requests = []
shell = """set -eu
test -z "${AQ_TEMPLATE_ENV_SENTINEL+x}"
test "$CARGO_INCREMENTAL" = 0
test "$CARGO_PROFILE_DEV_DEBUG" = line-tables-only
test "$CARGO_PROFILE_TEST_DEBUG" = line-tables-only
test "$(command -v cargo)" = /usr/local/bin/cargo
printf '%s\n' "$CARGO_INCREMENTAL" "$CARGO_PROFILE_DEV_DEBUG" "$CARGO_PROFILE_TEST_DEBUG" > proof.txt
command -v cargo >> proof.txt
cargo --version >> proof.txt
cat proof.txt
"""

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'gzip': raw = gzip.decompress(raw)
        requests.append(json.loads(raw))
        names = [tool.get('name') for tool in requests[0].get('tools', [])]
        if 'exec_command' in names:
            name, arguments = 'exec_command', {'cmd': shell, 'login': False, 'max_output_tokens': 300}
        elif 'shell_command' in names:
            name, arguments = 'shell_command', {'command': shell}
        else:
            assert 'shell' in names, names
            name, arguments = 'shell', {'command': ['/bin/bash', '-c', shell]}
        item = ({'type': 'function_call', 'id': 'tool-1', 'call_id': 'tool-1',
                 'name': name, 'arguments': json.dumps(arguments)} if len(requests) == 1 else
                {'type': 'message', 'id': 'msg-1', 'role': 'assistant',
                 'content': [{'type': 'output_text', 'text': 'done'}]})
        response_id = 'resp-' + str(len(requests))
        events = [
            {'type': 'response.created', 'response': {'id': response_id}},
            {'type': 'response.output_item.added', 'output_index': 0, 'item': item},
            {'type': 'response.output_item.done', 'output_index': 0, 'item': item},
            {'type': 'response.completed', 'response': {'id': response_id,
             'usage': {'input_tokens': 1, 'output_tokens': 1, 'total_tokens': 2}}},
        ]
        data = ''.join('event: ' + e['type'] + '\ndata: ' + json.dumps(e) + '\n\n' for e in events).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers(); self.wfile.write(data)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
args = [a.replace('<fixture>/work', str(work)) for a in json.loads(sys.argv[1])]
args[args.index('synthetic')] = 'gpt-5.1-codex'
args[-1:-1] = ['--skip-git-repo-check']
for setting in [
    'model_provider="fixture"',
    'model_providers.fixture.name="Offline environment fixture"',
    f'model_providers.fixture.base_url="http://127.0.0.1:{server.server_port}/v1"',
    'model_providers.fixture.wire_api="responses"',
    'model_providers.fixture.request_max_retries=0',
]:
    args[-1:-1] = ['--config', setting]
try:
    result = subprocess.run(['/opt/aq/harnesses/codex/codex', *args], cwd=work, env={
        'HOME': str(home), 'CODEX_HOME': str(home),
        'PATH': '/opt/aq/node/bin:/usr/bin:/bin', 'LANG': 'C.UTF-8',
        'CARGO_INCREMENTAL': '1', 'CARGO_PROFILE_DEV_DEBUG': '2',
        'CARGO_PROFILE_TEST_DEBUG': '2', 'AQ_TEMPLATE_ENV_SENTINEL': 'must-not-inherit',
    }, input='Execute the offline fixture tool command.', text=True, capture_output=True, timeout=30)
    print(json.dumps({'exit_code': result.returncode, 'stdout': result.stdout,
                      'stderr': result.stderr, 'provider_requests': len(requests)}), flush=True)
    assert result.returncode == 0
    proof = (work / 'proof.txt').read_text()
    assert proof.splitlines()[:4] == ['0', 'line-tables-only', 'line-tables-only', '/usr/local/bin/cargo']
    assert proof.splitlines()[4].startswith('cargo '), proof
    outputs = [item for request in requests for item in request.get('input', [])
               if item.get('type') == 'function_call_output']
    assert any('/usr/local/bin/cargo' in json.dumps(item.get('output')) for item in outputs), outputs
    print(json.dumps({'result': 'pass', 'tool_output': proof, 'ambient_sentinel_absent': True}))
finally:
    server.shutdown(); thread.join()
'''


def test_codex_tool_environment_in_rust_template():
    image = os.environ['TEMPLATE_ENV_IMAGE']
    golden = ROOT / 'crates/aq-worker/src/native/adapter/codex/goldens/base.json'
    arguments = json.loads(golden.read_text())['arguments']
    name = 'aq-template-env-' + uuid.uuid4().hex
    command = [
        'docker', 'run', '--rm', '--init', '--name', name,
        '--network', 'none', '--read-only', '--user', '10002:10002',
        '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges=true',
        '--security-opt', 'seccomp=' + str(ROOT / 'tools/qualification/seccomp/codex-bwrap-amd64.json'),
        '--pids-limit', '128', '--memory', '1g', '--cpus', '1',
        '--tmpfs', '/state:rw,nosuid,nodev,mode=1777,size=64m',
        '--tmpfs', '/tmp:rw,nosuid,nodev,mode=1777,size=64m',
        '--entrypoint', 'python3', image, '-c', PROBE, json.dumps(arguments),
    ]
    try:
        result = subprocess.run(command, text=True, capture_output=True, timeout=45)
        print(result.stdout, flush=True)
        assert result.returncode == 0, result.stderr
    finally:
        subprocess.run(['docker', 'rm', '-f', name], capture_output=True, timeout=10)
