#!/usr/bin/env python3
"""Describe installed bytes; build-time checks separately enforce input pins."""
import hashlib
import json
from pathlib import Path
import sys


def artifact(root, name, version, relative):
    path = root / relative
    with path.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    return {'name': name, 'version': version, 'path': '/opt/aq/' + relative,
            'sha256': digest, 'bytes': path.stat().st_size}


def manifest(root):
    pins = json.loads((root / 'pins.json').read_text())
    source = json.loads((root / 'source.json').read_text())
    inputs = pins['inputs']
    return {
        'schema_version': 1,
        'platform': pins['platform'],
        'source': source,
        'runner': artifact(root, 'aq-worker', source['runner_version'], 'bin/aq-worker'),
        'session_host': artifact(root, 'aq-session-host', source['session_host_version'], 'bin/aq-session-host'),
        'agent_client': artifact(root, 'aq-agent', source['agent_client_version'], 'bin/aq-agent'),
        'aq_git': artifact(root, 'aq-git', inputs['aq-git']['version'], 'bin/aq-git'),
        'node': artifact(root, 'node', inputs['node.tar.xz']['version'], 'node/bin/node'),
        'harnesses': [artifact(root, 'codex', inputs['codex']['version'], 'harnesses/codex/codex')],
        'proxies': {'responses': 'in-process:aq-worker', 'aq_loopback': 'in-process:aq-worker'},
    }


if __name__ == '__main__':
    root = Path(sys.argv[1])
    (root / 'manifest.json').write_text(json.dumps(manifest(root), indent=2, sort_keys=True) + '\n')
