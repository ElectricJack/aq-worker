"""Sanitize an explicitly selected real Codex rollout. Never discover files."""
import hashlib
import json
from pathlib import Path
import sys

def sanitize(source, destination):
    raw = Path(source).read_bytes()
    result = []
    counts = {}
    turns = {}
    calls = {}
    def identity(table, value, prefix):
        if value not in table:
            table[value] = f'{prefix}-{len(table) + 1}'
        return table[value]
    for line in raw.splitlines():
        row = json.loads(line)
        kind, payload = row.get('type'), row.get('payload', {})
        subtype = payload.get('type')
        key = kind if kind in ('session_meta', 'turn_context') else subtype
        if key not in ('session_meta', 'turn_context', 'task_started', 'task_complete',
                       'user_message', 'agent_message', 'token_count',
                       'function_call', 'function_call_output'):
            continue
        if counts.get(key, 0) >= (4 if key == 'token_count' else 1):
            continue
        if key == 'session_meta':
            safe = {'id': 'sanitized-h1-session', 'cli_version': payload['cli_version']}
        elif key == 'turn_context':
            safe = {'model': payload['model']}
        elif key == 'token_count':
            info = payload.get('info')
            if not info:
                continue
            safe_info = {}
            for field in ('total_token_usage', 'last_token_usage'):
                if isinstance(info.get(field), dict):
                    safe_info[field] = {k: v for k, v in info[field].items()
                        if k in ('input_tokens', 'cached_input_tokens', 'cache_write_input_tokens',
                                 'output_tokens', 'reasoning_output_tokens', 'total_tokens')
                        and type(v) is int and v >= 0}
            if type(info.get('model_context_window')) is int:
                safe_info['model_context_window'] = info['model_context_window']
            safe = {'type': key, 'info': safe_info}
        elif key in ('function_call', 'function_call_output'):
            safe = {'type': key, 'call_id': identity(calls, payload['call_id'], 'call')}
            if key == 'function_call':
                safe.update(name='sanitized_tool', arguments='[sanitized]')
            else:
                safe['output'] = '[sanitized]'
        else:
            safe = {'type': key}
            if key in ('user_message', 'agent_message'):
                safe['message'] = '[sanitized]'
        if payload.get('turn_id'):
            safe['turn_id'] = identity(turns, payload['turn_id'], 'turn')
        result.append({'timestamp': '2026-11-02T00:00:00.000Z', 'type': kind, 'payload': safe})
        counts[key] = counts.get(key, 0) + 1
    data = ''.join(json.dumps(row, separators=(',', ':')) + '\n' for row in result)
    Path(destination).write_text(data)
    print(json.dumps({'source_sha256': hashlib.sha256(raw).hexdigest(),
        'sanitized_sha256': hashlib.sha256(data.encode()).hexdigest(), 'counts': counts,
        'cli_version': result[0]['payload']['cli_version']}, indent=2))

if __name__ == '__main__':
    sanitize(sys.argv[1], sys.argv[2])
