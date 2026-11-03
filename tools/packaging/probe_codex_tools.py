#!/usr/bin/env python3
"""Run inside the native image: offline Codex -> code-mode host -> shell proof.

The local scripted Responses endpoint supplies one tool call, then checks its
returned output. No credentials, external network, or real model are involved.
"""
import gzip
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

MARKER = 'CODEX_NATIVE_TOOL_OK'


def main():
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers['Content-Length']))
            if self.headers.get('Content-Encoding') == 'gzip':
                raw = gzip.decompress(raw)
            requests.append(json.loads(raw))
            source = (
                'const cmd = "printf CODEX_NATIVE_TOOL_OK > proof.txt; cat proof.txt"; '
                'text(typeof tools.exec_command === "function" '
                '? await tools.exec_command({cmd, max_output_tokens: 100}) '
                ': await tools.shell_command({command: cmd}));'
            )
            item = (
                {'type': 'custom_tool_call', 'id': 'tool-1', 'call_id': 'tool-1',
                 'name': 'exec', 'input': source}
                if len(requests) == 1 else
                {'type': 'message', 'id': 'msg-1', 'role': 'assistant',
                 'content': [{'type': 'output_text', 'text': 'done'}]}
            )
            response_id = 'resp-' + str(len(requests))
            events = [
                {'type': 'response.created', 'response': {'id': response_id}},
                {'type': 'response.output_item.added', 'output_index': 0, 'item': item},
                {'type': 'response.output_item.done', 'output_index': 0, 'item': item},
                {'type': 'response.completed', 'response': {'id': response_id,
                 'usage': {'input_tokens': 1, 'output_tokens': 1, 'total_tokens': 2}}},
            ]
            data = ''.join('event: ' + e['type'] + '\ndata: ' + json.dumps(e) + '\n\n'
                           for e in events).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    with tempfile.TemporaryDirectory(prefix='codex-tool-probe-') as temporary:
        root = Path(temporary)
        (root / 'home').mkdir()
        (root / 'work').mkdir()
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        command = ['/opt/aq-runtime/codex', 'exec', '--json', '--skip-git-repo-check',
                   '--sandbox', 'danger-full-access', '-c', 'model="gpt-5.1-codex"',
                   '-c', 'model_provider="fixture"', '-c', 'features.code_mode=true',
                   '-c', 'features.code_mode_host=true',
                   '-c', 'model_providers.fixture.name="Offline packaging fixture"',
                   '-c', f'model_providers.fixture.base_url="http://127.0.0.1:{server.server_port}/v1"',
                   '-c', 'model_providers.fixture.wire_api="responses"',
                   '-c', 'model_providers.fixture.request_max_retries=0',
                   'Run the shell command provided by the offline fixture.']
        try:
            result = subprocess.run(command, cwd=root / 'work', env={
                'PATH': os.environ['PATH'], 'HOME': str(root / 'home'),
                'CODEX_HOME': str(root / 'home'), 'LANG': 'C.UTF-8'},
                input='', capture_output=True, text=True, timeout=30)
            print(json.dumps({'exit_code': result.returncode, 'stdout': result.stdout,
                              'stderr': result.stderr, 'provider_requests': len(requests)}))
            assert result.returncode == 0, 'Codex process failed'
            assert (root / 'work/proof.txt').read_text() == MARKER, 'shell proof missing'
            outputs = [item for request in requests for item in request.get('input', [])
                       if item.get('type') == 'custom_tool_call_output']
            assert any(MARKER in json.dumps(item.get('output')) for item in outputs), outputs
            print(json.dumps({'kind': 'native_codex_tool_execution', 'result': 'pass',
                              'provider': 'offline scripted Responses fixture', 'marker': MARKER}))
        finally:
            server.shutdown()
            thread.join()


if __name__ == '__main__':
    main()
