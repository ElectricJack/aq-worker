"""No process/network: closed opt-in validation before fixture config writes."""
import importlib.util
import tempfile
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'native_runner_fixture', Path(__file__).with_name('native-runner-fixture.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ReplayOptIn(unittest.TestCase):
    def packet(self, state, result=True):
        executor = {'kind': 'codex-0.166-trusted-node-named-readonly-result-local-v1'
                    if result else 'codex-0.166-trusted-node-named-readonly-v1',
                    'settings': {'host_root': str(state / 'host')}}
        native = {'executor': executor, 'state_dir': str(state),
                  'git_root': str(state / 'git'), 'git_socket': str(state / 'git.sock'),
                  'adapter_manifest': str(state / 'config/adapter.json'),
                  'inventory_definition': str(state / 'config/inventory.json'),
                  'capacity': {'disk_bytes': 1}}
        value = {'schema_version': 1, 'operation': 'configure',
                 'worker_config': 'http://127.0.0.1:1234/', 'native_config': native,
                 'installation_manifest': {}, 'inventory_definition': {}}
        if result:
            executor.update(client_manifest=str(state / 'config/client.json'),
                            composite_definition=str(state / 'config/composite.json'))
            value.update(client_manifest={}, composite_definition={})
        return value

    def test_normal_absence_and_explicit_result_opt_in(self):
        for replay in (False, True):
            with tempfile.TemporaryDirectory() as root:
                fixture.STATE = Path(root)
                packet = self.packet(fixture.STATE)
                if replay:
                    packet['fixture_replay_lost_b1_ack'] = True
                self.assertEqual(fixture.configure(packet, packet['worker_config']), replay)
                self.assertTrue((fixture.STATE / 'config/native.json').is_file())

    def test_bad_opt_in_refuses_before_any_file_write(self):
        for result, value in [(True, False), (True, 1), (True, 'true'), (False, True)]:
            with tempfile.TemporaryDirectory() as root:
                fixture.STATE = Path(root)
                packet = self.packet(fixture.STATE, result)
                packet['fixture_replay_lost_b1_ack'] = value
                with self.assertRaises(ValueError):
                    fixture.configure(packet, packet['worker_config'])
                self.assertEqual(list(fixture.STATE.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
