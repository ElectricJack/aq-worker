"""Offline checks for probe isolation, evidence retention and timeout cleanup."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location('codex_probe', ROOT / 'tools/qualification/probe-codex-docker.py')
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class CodexDockerProbe(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.output = Path(temporary.name)
        self.image = 'sha256:' + '1' * 64
        self.qualification = probe.Qualification(self.image, self.output)

    def saved(self):
        return json.loads((self.output / 'qualification.json').read_text())

    def test_profile_run_retains_command_and_digest(self):
        def execute(command):
            return dict(command=command, exit=0, stdout='canary passed', stderr='')

        with patch.object(probe, 'execute', side_effect=execute):
            self.qualification.run('canary', probe.PROFILE, mode='canary')
        report = self.saved()
        self.assertFalse(report['passed'])
        self.assertEqual(report['image_requested'], self.image)
        result, = report['results']
        self.assertEqual(result['profile_sha256'], hashlib.sha256(probe.PROFILE.read_bytes()).hexdigest())
        command = result['command']
        self.assertEqual(command[-6:], ['python3', self.image, '-c', probe.SETUP, 'canary', probe.CANARY])
        for flag, value in (('--user', '10001:10001'), ('--cap-drop', 'ALL'), ('--network', 'none'),
                            ('--pids-limit', '128'), ('--memory', '512m'), ('--cpus', '1')):
            self.assertEqual(command[command.index(flag) + 1], value)
        for flag in ('--rm', '--init', '--read-only'):
            self.assertIn(flag, command)
        self.assertIn('no-new-privileges=true', command)
        self.assertIn('seccomp=' + str(probe.PROFILE), command)
        self.assertNotIn('--mount', command)
        self.assertNotIn('-v', command)

    def test_failure_is_saved_before_asserting_exit_or_stderr(self):
        for code, stderr in ((2, 'expected denial'), (1, 'different denial')):
            with self.subTest(code=code), patch.object(probe, 'execute', return_value={
                    'exit': code, 'stdout': '', 'stderr': stderr}):
                with self.assertRaises(AssertionError):
                    self.qualification.run('negative-control', None, expected=1, error_text='expected denial')
            self.assertEqual(self.saved()['results'][-1]['exit'], code)
            self.assertFalse(self.saved()['passed'])

    def test_timeout_cleans_only_the_named_container_and_saves_evidence(self):
        calls = []

        def execute(command):
            calls.append(command)
            if len(calls) == 1:
                raise subprocess.TimeoutExpired(command, 30)
            return dict(command=command, exit=0, stdout='', stderr='')

        with patch.object(probe, 'execute', side_effect=execute):
            with self.assertRaises(subprocess.TimeoutExpired):
                self.qualification.run('timed-out', 'unconfined')
        container = calls[0][calls[0].index('--name') + 1]
        self.assertTrue(container.startswith('aq-codex-probe-'))
        self.assertEqual(calls[1], ['docker', 'rm', '-f', container])
        result, = self.saved()['results']
        self.assertEqual(result['timeout_seconds'], 30)
        self.assertEqual(result['cleanup']['command'], calls[1])
        self.assertFalse(self.saved()['passed'])

    def test_execute_keeps_thirty_second_default(self):
        result = subprocess.CompletedProcess(['probe'], 1, 'out', 'err')
        with patch.object(probe.subprocess, 'run', return_value=result) as run:
            self.assertEqual(probe.execute(['probe']), dict(command=['probe'], exit=1, stdout='out', stderr='err'))
        run.assert_called_once_with(['probe'], text=True, capture_output=True, timeout=30)


if __name__ == '__main__':
    unittest.main()
