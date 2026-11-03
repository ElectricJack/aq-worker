"""Offline checks of the E0 environment probe verdicts and retained evidence."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location(
    'e0_environment', ROOT / 'tools/qualification/probe-e0-native-environment.py')
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)
EVIDENCE = ROOT / 'docs/implementation/evidence/e0-native-inputs/environment-probe.json'


class EnvironmentProbe(unittest.TestCase):
    def setUp(self):
        self.data = EVIDENCE.read_bytes()
        self.report = json.loads(self.data)

    def test_retained_evidence_is_canonical_measured_and_passing(self):
        self.assertEqual(probe.canonical(self.report), self.data)
        self.assertEqual(self.report['authority'], probe.AUTHORITY)
        self.assertTrue(self.report['authority'].startswith('measured'))
        self.assertEqual(self.report['image_id'], probe.IMAGE)
        self.assertEqual(self.report['run_flags'], probe.RUN_FLAGS)
        checks, failed = probe.verify(self.report['facts'])
        self.assertEqual((checks, failed), (self.report['checks'], self.report['failed']))
        self.assertEqual(failed, [])
        self.assertTrue(self.report['passed'])

    def test_run_flags_keep_every_hardening_option(self):
        flags = ' '.join(probe.RUN_FLAGS)
        for required in ('--read-only', '--cap-drop ALL', 'no-new-privileges=true',
                         'seccomp=unconfined'):
            self.assertIn(required, flags)
        self.assertNotIn('--privileged', flags)
        self.assertNotIn('--cap-add', flags)
        self.assertNotIn('-v', probe.RUN_FLAGS + probe.PROBE_MOUNTS)

    def test_each_regression_is_refused(self):
        regressions = {
            'uid_10001': lambda f: f.update(uid=0),
            'capability_sets_empty': lambda f: f['status'].update(CapBnd='00000000a80425fb'),
            'no_new_privs': lambda f: f['status'].update(NoNewPrivs='0'),
            'docker_seccomp_profile_removed':
                lambda f: f['status'].update(Seccomp_filters=str(f['default_profile_seccomp_filters'])),
            'image_device_read_only': lambda f: f.update(image_mount_rows_read_only=False),
            'image_write_refused': lambda f: f.update(opt_write='allowed'),
            'program_pins': lambda f: f['programs']['codex'].update(bytes=1),
            'codex_0_166_0': lambda f: f.update(codex_version='codex-cli 0.167.0'),
            'tmux_3_4': lambda f: f.update(tmux_version='tmux 3.5'),
            'codex_sandbox_namespaces': lambda f: f.update(codex_sandbox_true_exit=1),
        }
        self.assertEqual(sorted(regressions), sorted(self.report['checks']))
        for name, mutate in regressions.items():
            with self.subTest(name):
                facts = copy.deepcopy(self.report['facts'])
                mutate(facts)
                self.assertEqual(probe.verify(facts)[1], [name])

    def test_evidence_is_named_by_its_digest_in_the_record(self):
        record = (ROOT / 'docs/implementation/E0-native-inputs.md').read_text()
        self.assertIn(hashlib.sha256(self.data).hexdigest(), record)


if __name__ == '__main__':
    unittest.main()
