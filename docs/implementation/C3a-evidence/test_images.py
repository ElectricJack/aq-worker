"""Physical C3a packaging evidence; run after building the local test composition."""
import json
import os
from pathlib import Path
import subprocess

IMAGE = os.environ.get('C3A_COMPOSED_IMAGE', 'aq-sandbox-c3a:after')
ROOT = Path(__file__).resolve().parents[3]


def run_image(*command, user='10002:10002'):
    return subprocess.check_output([
        'docker', 'run', '--rm', '--network', 'none', '--read-only',
        '--tmpfs', '/tmp:mode=1777', '--cap-drop', 'ALL',
        '--security-opt', 'no-new-privileges', '--user', user,
        '--entrypoint', command[0], IMAGE, *command[1:],
    ], text=True)


def test_installed_manifest_and_executables():
    pins = json.loads((ROOT / 'packaging/sandbox/pins.json').read_text())
    manifest = json.loads(run_image('/bin/cat', '/opt/aq/manifest.json'))
    assert manifest['schema_version'] == 1
    expected_source = json.loads((Path(__file__).parent / 'context-comparison.json').read_text())['source']
    assert manifest['source'] == expected_source
    artifacts = [manifest[key] for key in ('runner', 'session_host', 'aq_git', 'node')]
    artifacts += manifest['harnesses']
    for artifact in artifacts:
        observed = run_image('/usr/bin/sha256sum', artifact['path']).split()[0]
        assert observed == artifact['sha256']
        assert int(run_image('/usr/bin/stat', '-c', '%s', artifact['path'])) == artifact['bytes']
    assert manifest['harnesses'][0]['version'] == pins['inputs']['codex']['version']
    assert manifest['harnesses'][0]['sha256'] == pins['inputs']['codex']['sha256']
    assert manifest['aq_git']['sha256'] == pins['inputs']['aq-git']['sha256']
    assert run_image('/opt/aq/bin/codex', '--version').strip() == 'codex-cli ' + pins['inputs']['codex']['version']
    assert run_image('/opt/aq/bin/aq-git', '--version').strip() == 'aq-git ' + pins['inputs']['aq-git']['version']
    assert run_image('/opt/aq/bin/aq-worker', '--version').strip() == 'aq-worker ' + manifest['runner']['version']
    # Absolute private Node works even when a project's PATH has no Node.
    assert run_image('/usr/bin/env', 'PATH=/project-no-node', '/opt/aq/node/bin/node', '--version').strip() == 'v' + pins['inputs']['node.tar.xz']['version']
    for artifact in artifacts:
        result = subprocess.run([
            'docker', 'run', '--rm', '--network', 'none', '--entrypoint', '/usr/bin/ldd',
            IMAGE, artifact['path'],
        ], text=True, capture_output=True)
        assert 'not found' not in result.stdout + result.stderr
        assert result.returncode == 0 or 'not a dynamic executable' in result.stdout + result.stderr
    (Path(__file__).parent / 'installed-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


def test_os_users_git_tmux_and_bubblewrap():
    users = run_image('/usr/bin/getent', 'passwd', 'runner', 'agent')
    assert 'runner:x:10001:10001:' in users
    assert 'agent:x:10002:10002:' in users
    assert run_image('/opt/git/bin/git', '--version').strip() == 'git version 2.56.0'
    assert run_image('/usr/bin/tmux', '-V').startswith('tmux ')
    assert run_image('/usr/bin/bwrap', '--version').startswith('bubblewrap ')
    run_image('/bin/sh', '-ec', '''
        test -s /etc/ssl/certs/ca-certificates.crt
        /usr/bin/perl -e 'exit 0'
        mkdir /tmp/repo
        git -C /tmp/repo init --quiet
        printf public > /tmp/repo/file
        git -C /tmp/repo add file
        test -n "$(git -C /tmp/repo write-tree)"
        tmux -L c3a new-session -d -s smoke
        tmux -L c3a has-session -t smoke
        tmux -L c3a kill-server
    ''')
