import copy
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tomllib
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
PINS = json.loads((ROOT / 'scripts/e0-qual/pins.json').read_text())
VENDOR = 'vendor/aq-admission-0.1.13/src'


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


class MeasurementEvidence(unittest.TestCase):
    def setUp(self):
        self.d = load()
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.evidence = Path(temporary.name)
        model = dict(model='fixture-model', native_reasoning='high')
        self.stack = SimpleNamespace(PINS={'model': model}, layout=lambda: {'evidence': self.evidence},
                                     require=self.require)
        call = dict(exit_code=0, errors=[], prompt_utf8_bytes=100, prompt_sha256='fixture-digest',
                    payload_utf8_bytes=80, started_at_ms=1, finished_at_ms=2,
                    usage=dict(input_tokens=16484, cached_input_tokens=0, output_tokens=10,
                               reasoning_output_tokens=3),
                    rollout=dict(models=['fixture-model'], efforts=['high'], model_context_windows=[32768],
                                 last_input_tokens=16484))
        self.raw = dict(model, codex_version='codex-cli 0.166.0', calls=[
            dict(copy.deepcopy(call), call=name, finished_at_ms=i + 2)
            for i, name in enumerate(('minimal', 'mixed', 'adversarial'))])

    @staticmethod
    def require(condition, message):
        if not condition:
            raise ValueError(message)

    def measured(self):
        (self.evidence / 'measure-raw.json').write_text(json.dumps(self.raw))
        return self.d.measured(self.stack)

    def test_accepts_exact_counting_bound_and_retains_capacity_and_latest_time(self):
        rows, capacity, observed = self.measured()
        self.assertEqual((capacity, observed), (32768, 4))
        self.assertEqual([r['call'] for r in rows], ['minimal', 'mixed', 'adversarial'])
        self.assertTrue(all(r['input_tokens'] == r['bound_tokens'] == 16484 for r in rows))
        self.assertEqual(rows[0]['reasoning_output_tokens'], 3)
        self.assertEqual(rows[0]['rollout_model_context_windows'], [32768])

    def test_rejects_incomplete_or_unbounded_calls(self):
        original = copy.deepcopy(self.raw)
        for field, value in (('exit_code', 1), ('errors', ['incomplete'])):
            self.raw = copy.deepcopy(original)
            self.raw['calls'][0][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, 'incomplete'):
                self.measured()
        self.raw = copy.deepcopy(original)
        self.raw['calls'][0]['usage']['input_tokens'] += 1
        with self.assertRaisesRegex(ValueError, 'counting bound'):
            self.measured()

    def test_rejects_wrong_order_model_reasoning_or_capacity(self):
        original = copy.deepcopy(self.raw)
        self.raw['calls'].reverse()
        with self.assertRaisesRegex(ValueError, 'three bounded'):
            self.measured()
        for field, value in (('models', ['other']), ('efforts', ['low']),
                             ('model_context_windows', [65536]), ('model_context_windows', [])):
            self.raw = copy.deepcopy(original)
            self.raw['calls'][0]['rollout'][field] = value
            # Other calls still provide the single window when one call supplies no windows.
            if value == []:
                for call in self.raw['calls']:
                    call['rollout'][field] = []
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                self.measured()

    def test_public_bodies_refuse_changed_pinned_bytes(self):
        stack = SimpleNamespace(PINS=PINS, HERE=ROOT / 'scripts', require=self.require)
        builder = self.d.Builder()
        with patch.object(self.d.subprocess, 'run', return_value=SimpleNamespace(stdout=b'changed')):
            with self.assertRaisesRegex(ValueError, 'differs from the pinned I0 source'):
                self.d.pinned_public_bodies(stack, builder)
        self.assertEqual(builder.definitions, [])


if __name__ == '__main__':
    unittest.main()
