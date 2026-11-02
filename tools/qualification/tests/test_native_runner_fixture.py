"""Hostless fixture-protocol checks. Never runs worker/Git/Codex or namespaces."""
import importlib.util
import json
from pathlib import Path
import socket
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location('native_runner_fixture', ROOT / 'scripts/native-runner-fixture.py')
fixture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixture)


class FixtureProtocol(unittest.TestCase):
    def test_packet_is_one_closed_bounded_object_with_no_duplicate_fields(self):
        for payload, accepted in [(b'{"schema_version":1}', True),
                                  (b'{"schema_version":1,"schema_version":2}', False),
                                  (b'[]', False), (b'x' * (fixture.MAX_PACKET + 1), False)]:
            left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
            try:
                left.send(payload)
                if accepted:
                    self.assertEqual(fixture.receive(right), {'schema_version': 1})
                else:
                    with self.assertRaises((ValueError, json.JSONDecodeError)):
                        fixture.receive(right)
            finally:
                left.close()
                right.close()

    def test_known_private_config_files_only_and_ordinary_runner_remains_validator(self):
        original = fixture.STATE
        with tempfile.TemporaryDirectory() as directory:
            fixture.STATE = Path(directory)
            native = {'state_dir': directory, 'git_root': directory + '/git',
                      'git_socket': directory + '/git.sock', 'adapter_manifest': directory + '/config/adapter.json',
                      'inventory_definition': directory + '/config/inventory.json',
                      'executor': {'kind': 'codex-0.166-trusted-node-named-readonly-v1',
                                   'settings': {'host_root': directory + '/host'}},
                      'capacity': {'disk_bytes': 1}}
            packet = {'schema_version': 1, 'operation': 'configure',
                      'worker_config': 'server_url="http://127.0.0.1:1234/"',
                      'native_config': native, 'installation_manifest': {'closed': 'fixture'},
                      'inventory_definition': {'closed': 'fixture'}}
            try:
                wrong = json.loads(json.dumps(packet))
                wrong['native_config']['adapter_manifest'] = '/outside'
                with self.assertRaises(ValueError):
                    fixture.configure(wrong, 'http://127.0.0.1:1234/')
                self.assertEqual(list(Path(directory).iterdir()), [])
                fixture.configure(packet, 'http://127.0.0.1:1234/')
                actual = Path(directory) / 'config'
                self.assertEqual({p.name for p in actual.iterdir()},
                                 {'worker.toml', 'native.json', 'adapter.json', 'inventory.json'})
                for path in actual.iterdir():
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                with self.assertRaises(FileExistsError):
                    fixture.private_file(actual / 'worker.toml', b'overwrite')
            finally:
                fixture.STATE = original


    def test_result_profile_validates_all_files_before_effects_and_writes_canonical_json(self):
        original = fixture.STATE
        with tempfile.TemporaryDirectory() as directory:
            fixture.STATE = Path(directory)
            native = {'state_dir': directory, 'git_root': directory + '/git',
                      'git_socket': directory + '/git.sock', 'adapter_manifest': directory + '/config/adapter.json',
                      'inventory_definition': directory + '/config/inventory.json',
                      'executor': {'kind': 'codex-0.166-trusted-node-named-readonly-result-local-v1',
                                   'settings': {'host_root': directory + '/host'},
                                   'client_manifest': directory + '/config/client.json',
                                   'composite_definition': directory + '/config/composite.json'},
                      'capacity': {'disk_bytes': 1}}
            packet = {'schema_version': 1, 'operation': 'configure',
                      'worker_config': 'server_url="http://127.0.0.1:1234/"',
                      'native_config': native, 'installation_manifest': {'closed': 'fixture'},
                      'inventory_definition': {'closed': 'fixture'},
                      'client_manifest': {'z': 2, 'a': 1}, 'composite_definition': {'profile': 'fixture'}}
            try:
                for mutate in ('path', 'size'):
                    wrong = json.loads(json.dumps(packet))
                    if mutate == 'path':
                        wrong['native_config']['executor']['client_manifest'] = '/outside'
                    else:
                        wrong['composite_definition']['oversized'] = 'x' * 65536
                    with self.assertRaises(ValueError):
                        fixture.configure(wrong, 'http://127.0.0.1:1234/')
                    self.assertEqual(list(Path(directory).iterdir()), [])
                fixture.configure(packet, 'http://127.0.0.1:1234/')
                actual = Path(directory) / 'config'
                self.assertEqual(len(list(actual.iterdir())), 6)
                self.assertEqual((actual / 'client.json').read_bytes(), b'{"a":1,"z":2}')
                self.assertTrue(all(path.stat().st_mode & 0o777 == 0o600 for path in actual.iterdir()))
            finally:
                fixture.STATE = original

if __name__ == '__main__':
    unittest.main()
