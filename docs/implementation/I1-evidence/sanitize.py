"""Sanitize an explicitly selected real Claude Code session. Never discover files.

Keeps record order, record kinds, attachment subtypes, tool names, models,
stop reasons and the numeric per-call usage counters. Replaces the session,
row, message and tool-call identifiers. Drops every prompt, text, thinking,
tool input and tool output, and the cwd, git branch, timestamps, request
shapes and bodies.
"""
import hashlib
import json
from pathlib import Path
import sys

SESSION = 'sanitized-i1-session'
COUNTERS = ('input_tokens', 'output_tokens', 'cache_read_input_tokens',
            'cache_creation_input_tokens')


def sanitize(source, destination):
    raw = Path(source).read_bytes()
    ids = {}
    counts = {}

    def identity(value, prefix):
        if value not in ids:
            ids[value] = f'{prefix}-{len(ids) + 1}'
        return ids[value]

    def block(b):
        kind = b['type']
        if kind == 'text':
            return {'type': kind, 'text': '[sanitized]'}
        if kind in ('thinking', 'redacted_thinking'):
            return {'type': kind}
        if kind == 'tool_use':
            return {'type': kind, 'id': identity(b['id'], 'toolu'), 'name': b['name'], 'input': {}}
        if kind == 'tool_result':
            return {'type': kind, 'tool_use_id': identity(b['tool_use_id'], 'toolu'),
                    'content': '[sanitized]'}
        raise SystemExit(f'unknown content block {kind!r}')

    result = []
    for line in raw.splitlines():
        if not line:
            continue
        row = json.loads(line)
        kind = row['type']
        counts[kind] = counts.get(kind, 0) + 1
        safe = {'type': kind, 'sessionId': SESSION}
        if kind == 'attachment':
            safe['attachment'] = {'type': row['attachment']['type']}
        elif kind in ('user', 'assistant'):
            message = row['message']
            safe['uuid'] = identity(row['uuid'], 'row')
            out = {'role': message['role']}
            if kind == 'assistant':
                out['id'] = identity(message['id'], 'msg')
                out['model'] = message['model']
                out['stop_reason'] = message.get('stop_reason')
                usage = message.get('usage')
                if usage is not None:
                    out['usage'] = {k: usage[k] for k in COUNTERS
                                    if type(usage.get(k)) is int and usage[k] >= 0}
            content = message.get('content')
            if isinstance(content, str):
                out['content'] = '[sanitized]'
            elif isinstance(content, list):
                out['content'] = [block(b) for b in content]
            safe['message'] = out
        result.append(safe)
    data = ''.join(json.dumps(r, separators=(',', ':')) + '\n' for r in result)
    Path(destination).write_text(data)
    print(json.dumps({'source_sha256': hashlib.sha256(raw).hexdigest(),
                      'fixture_sha256': hashlib.sha256(data.encode()).hexdigest(),
                      'records': counts}, indent=2))


if __name__ == '__main__':
    sanitize(sys.argv[1], sys.argv[2])
