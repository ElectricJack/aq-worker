import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/opencode-enroll.py'
LOGIN = {'openai': {'type': 'oauth', 'refresh': 'rt-secret-value', 'access': 'at-secret-value',
                    'expires': 1893456000000, 'accountId': 'acct'}}


class OpenCodeEnroll(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.tmpdir = self.root / 'tmp'
        self.tmpdir.mkdir()
        self.out = self.root / 'login.json'

    def fake(self, login, exit_code=0):
        material = self.root / 'material.json'
        material.write_text(json.dumps(login) if isinstance(login, dict) else login)
        fake = self.root / 'opencode'
        fake.write_text(
            '#!/bin/sh\n'
            f'[ "$*" = "auth login -p openai -m ChatGPT Pro/Plus (headless)" ] || exit 9\n'
            '[ -z "$OPENAI_API_KEY" ] || exit 8\n'
            'mkdir -p "$XDG_DATA_HOME/opencode"\n'
            f'cp {material} "$XDG_DATA_HOME/opencode/auth.json"\n'
            f'exit {exit_code}\n')
        fake.chmod(0o755)
        return fake

    def run_enroll(self, fake):
        env = dict(os.environ, TMPDIR=str(self.tmpdir), OPENAI_API_KEY='must-not-leak')
        return subprocess.run(
            [sys.executable, '-I', '-B', str(SCRIPT), '--opencode', str(fake), '--out', str(self.out)],
            env=env, capture_output=True, text=True, check=False)

    def test_capture_writes_private_material_and_prints_no_token(self):
        result = self.run_enroll(self.fake(LOGIN))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.out.read_text()), LOGIN)
        self.assertEqual(stat.S_IMODE(self.out.stat().st_mode), 0o600)
        report = json.loads(result.stdout)
        self.assertEqual(report['kind'], 'opencode_chatgpt_auth')
        self.assertEqual(report['expires'], '2030-01-01T00:00:00+00:00')
        self.assertNotIn('secret', result.stdout + result.stderr)
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_refuses_anything_but_one_openai_oauth_login(self):
        api = {'openai': {'type': 'api', 'key': 'sk-secret'}}
        extra = dict(LOGIN, anthropic={'type': 'api', 'key': 'secret'})
        field = {'openai': dict(LOGIN['openai'], extra='x')}
        for login in (api, extra, field, '{"openai":{}, "openai":{}}', 'not json'):
            with self.subTest(login=login):
                result = self.run_enroll(self.fake(login))
                self.assertEqual(result.returncode, 1)
                self.assertFalse(self.out.exists())
                self.assertNotIn('secret', result.stdout + result.stderr)
                self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_failed_login_and_existing_output_are_refused(self):
        self.assertEqual(self.run_enroll(self.fake(LOGIN, exit_code=3)).returncode, 1)
        self.assertFalse(self.out.exists())
        self.out.write_text('keep')
        self.assertEqual(self.run_enroll(self.fake(LOGIN)).returncode, 2)
        self.assertEqual(self.out.read_text(), 'keep')


if __name__ == '__main__':
    unittest.main()
