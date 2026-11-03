import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
PINS = json.loads((ROOT / 'scripts/e0-qual/pins.json').read_text())
VENDOR = 'vendor/aq-admission-0.1.11/src'


def load():
    spec = importlib.util.spec_from_file_location('e0_qual_definitions', ROOT / 'scripts/e0-qual/definitions.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def pinned_source(path):
    revision = PINS['worker_i0']['source_revision']
    result = subprocess.run(['git', '-C', str(ROOT), 'show', f'{revision}:{path}'], capture_output=True, text=True)
    if result.returncode != 0:
        raise unittest.SkipTest(f'pinned I0 source {revision} is not in this clone')
    return result.stdout


class E0QualDefinitions(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.d = load()

    def test_negotiation_features_match_pinned_refresh_contract(self):
        # The I0 worker refuses native.json unless every required refresh-v1 phase feature is negotiated.
        shared = pinned_source(f'{VENDOR}/preparation_shared.rs')
        refresh = pinned_source(f'{VENDOR}/preparation_refresh_v1.rs')
        base = re.search(r'pub fn required_features\(\) -> Vec<PhaseFeature> \{\s*vec!\[(.*?)\$\(', shared, re.S)
        extra = re.search(r'extra_features:\s*\[([^\]]*)\]', refresh)
        self.assertIsNotNone(base)
        self.assertIsNotNone(extra)
        names = re.findall(r'PhaseFeature::(\w+)', base.group(1)) + re.findall(r'\w+', extra.group(1))
        self.assertEqual(sorted(self.d.snake(n) for n in names), sorted(self.d.NEGOTIATION_FEATURES))
        self.assertEqual(len(set(self.d.NEGOTIATION_FEATURES)), len(self.d.NEGOTIATION_FEATURES))

    def test_long_contract_is_the_refresh_interactive_long_contract(self):
        refresh = pinned_source(f'{VENDOR}/preparation_refresh_v1.rs')
        contract_id = re.search(r'contract_id:\s*"([^"]+)"', refresh).group(1)
        self.assertEqual(self.d.LONG_CONTRACT['id'], f'{contract_id}.interactive-long-v1')

    def test_toml_value_round_trips_and_refuses_unrepresentable(self):
        value = {'a': 1, 'b': 'x"y', 'c': [True, 2], 'd': None, 'e': {'f': 'g'}}
        parsed = tomllib.loads('t = ' + self.d.toml_value(value))['t']
        self.assertEqual(parsed, {'a': 1, 'b': 'x"y', 'c': [True, 2], 'e': {'f': 'g'}})
        for bad in (1.5, 'del\x7f'):
            with self.assertRaises(ValueError):
                self.d.toml_value(bad)

    def test_canonical_is_sorted_compact_utf8(self):
        self.assertEqual(self.d.canonical({'b': 1, 'a': 'é'}), '{"a":"é","b":1}')


if __name__ == '__main__':
    unittest.main()
