"""Synthetic observer tests; these do not qualify a real runtime or model."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location('e0_inputs', ROOT / 'scripts/observe-e0-native-inputs.py')
observer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(observer)


class OwnerAttestation(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.auth = self.root / 'auth.json'
        self.auth.write_text(json.dumps({'auth_mode': 'chatgpt', 'tokens': {
            'access_token': 'SECRET_ACCESS', 'refresh_token': 'SECRET_REFRESH',
            'account_id': 'PRIVATE_ACCOUNT'}, 'unknown': 'SECRET_UNKNOWN'}))
        self.auth.chmod(0o600)

    def test_private_observation_has_only_presence_metadata(self):
        before = self.auth.read_bytes()
        report = observer.auth_observation(self.auth)
        self.assertEqual(report['auth_mode'], 'chatgpt')
        self.assertFalse(report['lease_input_created'])
        self.assertEqual(report['authentication_validity'], 'not tested')
        encoded = json.dumps(report)
        for forbidden in ('SECRET', 'PRIVATE_ACCOUNT', 'access_token', 'refresh_token', 'sha256'):
            self.assertNotIn(forbidden, encoded)
        self.assertEqual(before, self.auth.read_bytes())

    def test_auth_refuses_symlink_permissions_fifo_empty_and_duplicate_inputs(self):
        link = self.root / 'link.json'
        link.symlink_to(self.auth)
        with self.assertRaises(OSError):
            observer.auth_observation(link)
        self.auth.chmod(0o644)
        with self.assertRaisesRegex(ValueError, '0600'):
            observer.auth_observation(self.auth)
        self.auth.chmod(0o600)
        fifo = self.root / 'fifo'
        os.mkfifo(fifo, 0o600)
        with self.assertRaisesRegex(ValueError, 'regular'):
            observer.auth_observation(fifo)
        for data in ('', '{"tokens":"SECRET",bad}',
                     '{"auth_mode":"chatgpt","auth_mode":"apikey","tokens":{"x":"SECRET"}}',
                     '{"auth_mode":"chatgpt","tokens":{}}'):
            self.auth.write_text(data)
            with self.subTest(data_length=len(data)), self.assertRaises(ValueError) as error:
                observer.auth_observation(self.auth)
            self.assertNotIn('SECRET', str(error.exception))

    def test_output_is_fresh_private_and_cannot_replace_evidence(self):
        path = self.root / 'report.json'
        observer.write_report(path, {'authority': observer.AUTHORITY})
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            observer.write_report(path, {'replacement': True})
        self.assertEqual(path.read_bytes(), original)
        link = self.root / 'report-link.json'
        link.symlink_to(path)
        with self.assertRaises(FileExistsError):
            observer.write_report(link, {})
        self.assertEqual(path.read_bytes(), original)

    def test_image_probe_is_immutable_networkless_without_secret_mounts_or_relaxation(self):
        image = 'sha256:' + '1' * 64
        calls = []
        def run(argv, timeout=30):
            calls.append(argv)
            if argv[1] == 'image':
                return json.dumps([{'Id': image, 'Os': 'linux', 'Architecture': 'amd64',
                                    'Config': {'Labels': {'org.opencontainers.image.revision': '2' * 40}}}]).encode()
            return json.dumps({'source_revision': '2' * 40, 'programs': {}, 'codex_version': '0.166.0'}).encode()
        with patch.object(observer, 'run', side_effect=run):
            result = observer.image_observation(image)
        self.assertEqual(result['image_id'], image)
        argv = calls[1]
        for argument in ('--pull=never', '--network=none', '--read-only', '--cap-drop=ALL',
                         '--security-opt=no-new-privileges', '--user=10001:10001'):
            self.assertIn(argument, argv)
        self.assertFalse(any(v in argv for v in ('-v', '--mount', '--privileged', '--env-file')))
        self.assertNotIn('seccomp=unconfined', ' '.join(argv))
        with self.assertRaisesRegex(ValueError, 'immutable'):
            observer.image_observation('mutable:latest')

        # I0 predates packaged build.json: retain that absence, never invent provenance.
        replies = [json.dumps([{'Id': image, 'Os': 'linux', 'Architecture': 'amd64',
                                'Config': {'Labels': {'org.opencontainers.image.revision': '2' * 40}}}]).encode(),
                   json.dumps({'source_revision': None, 'programs': {}, 'codex_version': '0.166.0'}).encode()]
        with patch.object(observer, 'run', side_effect=replies):
            self.assertFalse(observer.image_observation(image)['build_metadata_present'])
        replies[1] = json.dumps({'source_revision': '3' * 40, 'programs': {}, 'codex_version': '0.166.0'}).encode()
        with patch.object(observer, 'run', side_effect=replies), \
             self.assertRaisesRegex(ValueError, 'differs'):
            observer.image_observation(image)

    def test_failed_commands_do_not_disclose_child_output(self):
        failure = subprocess.CompletedProcess([], 1, b'SECRET_OUT', b'SECRET_ERR')
        with patch.object(observer.subprocess, 'run', return_value=failure):
            with self.assertRaises(ValueError) as error:
                observer.run(['unused'])
        self.assertNotIn('SECRET', str(error.exception))

    def test_collect_labels_every_record_and_keeps_missing_qualification_blocked(self):
        with patch.object(observer, 'run', return_value=b'codex-cli 0.166.0\n'), \
             patch.object(observer, 'image_observation', return_value={'image_id': 'synthetic-test'}), \
             patch.object(observer, 'e0_observation', return_value={'verification_scope': 'saved receipts only'}):
            report = observer.collect(self.root, self.auth, Path('/codex-test'), 'unused')
        self.assertEqual(report['authority'], observer.AUTHORITY)
        self.assertTrue(all(r['authority'] == observer.AUTHORITY for r in report['records']))
        self.assertFalse(report['native_configuration_emitted'])
        self.assertEqual(report['status'], 'blocked_missing_owner_qualification')
        self.assertIn('counting_evidence', report['missing_owner_inputs']['context'])
        self.assertIn('scoped_presence', report['missing_owner_inputs']['qualification'])
        self.assertNotIn('SECRET', json.dumps(report))

    def test_e0_saved_receipts_must_agree_and_are_not_live_verification(self):
        (self.root / 'state/vault').mkdir(parents=True)
        (self.root / 'source/scripts').mkdir(parents=True)
        seed = {'designation': {'base_branch': 'main', 'credential_ref': 'none',
                               'local_fixture': False,
                               'url': 'https://github.com/ElectricJack/aq-scratch.git'},
                'repository_id': 'synthetic-test-repository'}
        retention = {'id': 'synthetic-test-handle', 'state': 'active',
                     'repository_id': seed['repository_id'], 'binding_generation': 1,
                     'refs': [{'oid': 'synthetic-test-base'}]}
        evidence = {'seed': seed, 'source_revision': 'synthetic-test-revision',
                    'graph_task_verified': True, 'repository': {'binding_generation': 1},
                    'source_base': {'retention': retention, 'retention_handle': retention['id'],
                                    'base': {'algorithm': 'sha1', 'hex': 'synthetic-test-base'}}}
        for name, data in [('seed-evidence.json', evidence), ('state/seed.json', seed),
                           ('source-base-exchanges.json', [
                               {'request': {'operation': 'retention.inspect'},
                                'response': {'ok': True, 'result': retention}}]),
                           ('source/scripts/dev-catalog.json', {'catalog': {'intelligence': []}})]:
            (self.root / name).write_text(json.dumps(data))
        (self.root / 'state/vault/config.toml').write_text('port = 1\n')
        report = observer.e0_observation(self.root)
        self.assertIn('live graph and retention not rechecked', report['verification_scope'])
        self.assertFalse(report['configuration_sections_present']['intelligence'])
        seed['designation']['local_fixture'] = True
        (self.root / 'state/seed.json').write_text(json.dumps(seed))
        with self.assertRaisesRegex(ValueError, 'seed receipt mismatch'):
            observer.e0_observation(self.root)


if __name__ == '__main__':
    unittest.main()
