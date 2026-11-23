"""Behavior tests for project environment recipe parsing."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_template_tool as fixtures  # noqa: E402  (archive and config builders)

TOOLS = Path(__file__).resolve().parents[2] / 'packaging/sandbox'
_spec = importlib.util.spec_from_file_location('environment_recipe', TOOLS / 'environment_recipe.py')
recipe = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(recipe)
parse_environment = recipe.parse_environment

LIMITS = {'max_install_steps': 64, 'max_step_args': 32, 'max_arg_bytes': 4096, 'max_prefetch': 16,
          'max_recipe_bytes': 1048576, 'build_wall_ms': 3600000, 'build_cpu_millis': 4000,
          'build_memory_bytes': 8589934592, 'log_bytes': 65536, 'try_concurrency': 2, 'keep_versions': 3}

NODE = ('sh -c "curl -fsSL https://nodejs.org/dist/v24.1.0/node-v24.1.0-linux-x64.tar.xz -o /tmp/node.tar.xz '
        '&& echo \'' + 'a' * 64 + '  /tmp/node.tar.xz\' | sha256sum -c - '
        '&& tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 && rm /tmp/node.tar.xz"')

WEB = f'''---
name: web
default: true
provides: [node, pnpm, playwright, python]
---
## Install
- {NODE}
- sh -c "corepack enable && corepack prepare pnpm@10.34.5 --activate"
- sh -c "npx playwright@1.64.1 install --with-deps chromium"

## Check
- node --version
- pnpm --version
- npx playwright --version

## Warm
- pnpm install --frozen-lockfile
'''


class RecipeTests(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.root = Path(self._directory.name)

    def write(self, text):
        path = self.root / 'recipe.md'
        path.write_bytes(text.encode() if isinstance(text, str) else text)
        return path

    def parse(self, text, **limits):
        return parse_environment(self.write(text), {**LIMITS, **limits})

    def refuses(self, text, pattern='', **limits):
        with self.assertRaisesRegex(ValueError, pattern):
            self.parse(text, **limits)

    def test_parses_front_matter_and_sections(self):
        env = self.parse(WEB)
        self.assertEqual(list(env), ['name', 'default', 'provides', 'install_steps', 'checks', 'warm', 'sha256'])
        self.assertTrue(env['name'] == 'web' and env['default'] is True)
        self.assertEqual(env['provides'], ['node', 'pnpm', 'playwright', 'python'])
        self.assertEqual(len(env['install_steps']), 3)
        self.assertEqual(env['install_steps'][0][:2], ['sh', '-c'])
        self.assertIn('sha256sum -c', env['install_steps'][0][2])
        self.assertEqual(env['install_steps'][1],
                         ['sh', '-c', 'corepack enable && corepack prepare pnpm@10.34.5 --activate'])
        self.assertEqual(env['checks'], ['node --version', 'pnpm --version', 'npx playwright --version'])
        self.assertEqual(env['warm'], ['pnpm install --frozen-lockfile'])
        import hashlib
        self.assertEqual(env['sha256'], hashlib.sha256(WEB.encode()).hexdigest())

    def test_optional_parts_default(self):
        env = self.parse('---\nname: lib\n---\n## Install\n- true\n')
        self.assertEqual(env, {'name': 'lib', 'default': False, 'provides': [], 'install_steps': [['true']],
                               'checks': [], 'warm': [], 'sha256': env['sha256']})

    def test_refuses_a_parent(self):  # spec 6.1: AQ supplies it
        self.refuses(WEB.replace('default: true', 'parent: x@sha256:' + 'a' * 64), 'parent')

    def test_bounds_come_from_limits(self):
        self.refuses(WEB, 'install steps', max_install_steps=2)
        self.refuses(WEB, 'arguments', max_step_args=2)
        self.refuses(WEB, 'invalid', max_arg_bytes=20)
        self.refuses(WEB, 'byte limit', max_recipe_bytes=100)
        self.refuses(WEB, 'limit', max_recipe_bytes=0)
        self.parse(WEB, max_install_steps=3)

    def test_refuses_unknown_front_matter_duplicate_sections_and_bad_names(self):
        body = '## Install\n- true\n'
        self.refuses('---\nname: web\nowner: me\n---\n' + body, 'unknown front matter')
        self.refuses('---\nname: web\nname: web\n---\n' + body, 'duplicate front matter')
        self.refuses('---\nname: web\n---\n' + body + '## Install\n- true\n', 'duplicate recipe section')
        self.refuses('---\nname: web\n---\n' + body + '## Check\n- a\n## Check\n- b\n', 'duplicate recipe section')
        self.refuses('---\nname: web\n---\n' + body + '## Extras\n- a\n', 'unknown recipe section')
        self.refuses('---\nname: web\n---\n## Check\n- true\n', 'Install')
        self.refuses('---\ndefault: true\n---\n' + body, 'name')
        self.refuses('---\nname: web\ndefault: yes\n---\n' + body, 'default')
        for bad in ['Web', 'a/b', '1web', 'web_x', '', 'a' * 65]:
            self.refuses(f'---\nname: {bad}\n---\n' + body)
        for bad in ['a/b', 'a b', 'node,,pnpm', 'a' * 129]:
            self.refuses(f'---\nname: web\nprovides: [node, {bad}]\n---\n' + body)
        self.refuses('---\nname: web\nprovides: [node, node]\n---\n' + body, 'duplicate provides')
        self.refuses('---\nname: web\nprovides: node\n---\n' + body, 'list')
        self.refuses('name: web\n' + body, 'front matter')
        self.refuses('---\nname: web\n' + body, 'front matter')
        self.refuses('---\nname: web\n---\nprose\n' + body, 'list item')
        self.refuses('---\nname: web\n---\n' + body.replace('- true', 'true'), 'list item')

    def test_refuses_bad_steps_control_characters_and_secrets(self):
        head = '---\nname: web\n---\n## Install\n'
        self.refuses(head + '- sh -c "unterminated\n')
        self.refuses(head + '- -rf /\n', 'executable')
        self.refuses(head + '- true\x01\n', 'invalid')
        self.refuses(head + '- true\r\n', 'invalid')
        self.refuses(head + '- true\t\n', 'invalid')
        self.refuses(head + '-   \n')
        self.refuses(head + '- true\n## Check\n- echo TOKEN=private-value\n', 'secret')
        self.refuses(head + '- sh -c "export API_KEY=private-value"\n', 'secret')
        self.refuses(head + '- true\n## Warm\n- echo password=private-value\n', 'secret')
        self.refuses(b'---\nname: web\n---\n## Install\n- true \xff\n', 'UTF-8')


RECIPE = """---
name: web
provides: [node]
---
## Install
- sh -c "mkdir -p /tools"
## Check
- node --version
- pnpm --version
## Warm
- pnpm install --frozen-lockfile
"""

# A fake docker that answers each buildx target from behavior.json and records every call.
FAKE_DOCKER = """#!/usr/bin/python3
import json, pathlib, shutil, sys
args = sys.argv[1:]
root = pathlib.Path(__file__).parent
behavior = json.loads((root / 'behavior.json').read_text())
(root / 'calls').open('a').write(json.dumps(args) + '\\n')
assert 'OPENAI_API_KEY' not in __import__('os').environ
if args[:3] == ['--context', 'build-desktop', 'run']:
    sys.stderr.write(behavior.get('publish_error', ''))
    sys.exit(1 if behavior.get('publish_error') else 0)
assert args[:6] == ['--context', 'build-desktop', 'buildx', 'build', '--builder', 'aq-templates']
target = args[args.index('--target') + 1]
output = args[args.index('--output') + 1]
context = pathlib.Path(args[-1])
if '--file' in args:
    definition = pathlib.Path(args[args.index('--file') + 1])
    (root / 'ignored').write_text(definition.with_name('Dockerfile.dockerignore').read_text())
    (root / 'context').write_text(str(context))
else:
    definition = context / 'Dockerfile'
    assert sorted(p.name for p in context.iterdir()) == ['Dockerfile']
(root / ('rendered-' + target)).write_text(definition.read_text())
(root / 'targets').open('a').write(target + '\\n')
sys.stderr.write(behavior.get('noise', ''))
if behavior.get('fail') == target:
    sys.stderr.write(behavior.get('error', ''))
    sys.exit(1)
dest = output.split('dest=', 1)[1].split(',', 1)[0]
if target == 'template':
    shutil.copyfile(root / 'fixture.tar', dest)
else:
    pathlib.Path(dest).mkdir()
    checks = pathlib.Path(dest) / 'checks' if target == 'try-output' else pathlib.Path(dest)
    checks.mkdir(exist_ok=True)
    for number, (out, code) in enumerate(behavior['checks']):
        (checks / f'{number}.out').write_text(out)
        (checks / f'{number}.exit').write_text(f'{code}\\n')
    if target == 'try-output' and 'run' in behavior:
        (pathlib.Path(dest) / 'run').mkdir()
        (pathlib.Path(dest) / 'run/0.out').write_text(behavior['run'][0])
        (pathlib.Path(dest) / 'run/0.exit').write_text(f"{behavior['run'][1]}\\n")
"""


class Fixture:
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.root = Path(self._directory.name)
        self.tool = fixtures.load('template_tool')
        self.digest = fixtures.oci(self.root / 'fixture.tar')
        (self.root / 'docker').write_text(FAKE_DOCKER)
        (self.root / 'docker').chmod(0o755)
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]])
        (self.root / 'recipe.md').write_text(RECIPE)
        self.env = parse_environment(self.root / 'recipe.md', LIMITS)

    def behave(self, **behavior):
        (self.root / 'behavior.json').write_text(json.dumps(behavior))

    def config(self, **limits):
        return {**fixtures.config(), 'docker_binary': str(self.root / 'docker'),
                'base_template': 'registry.example/aq/base@sha256:' + 'd' * 64,
                'limits': {**LIMITS, **limits}}


class BuildEnvironmentTests(Fixture, unittest.TestCase):
    def build(self, name='out', **limits):
        return self.tool.build_environment(self.env, self.config(**limits), self.root / name)

    def fails(self, step, name='out', **limits):
        with self.assertRaises(self.tool.BuildFailed) as caught:
            self.build(name, **limits)
        self.assertEqual(caught.exception.step, step)
        return caught.exception.log

    def test_check_output_is_recorded_as_toolchain(self):
        build = self.build()
        self.assertEqual(build['toolchain'], [{'command': 'node --version', 'output': 'v24.22.3\n'},
                                              {'command': 'pnpm --version', 'output': '10.34.5\n'}])
        self.assertEqual(build['digest'], self.digest)
        self.assertEqual(build['image'], 'registry.example/aq/template@' + self.digest)
        self.assertEqual(build['recipe_sha256'], self.env['sha256'])
        self.assertEqual((build['name'], build['provides'], build['warm']),
                         ('web', ['node'], ['pnpm install --frozen-lockfile']))
        self.assertEqual(build['harnesses'], [{'harness': 'codex', 'version': '0.166.0'}])
        self.assertEqual(list(build), ['name', 'default', 'digest', 'image', 'recipe_sha256', 'provides',
                                       'toolchain', 'warm', 'harnesses'])
        self.assertIs(build['default'], False)
        self.assertTrue((self.root / 'out/image.oci.tar').is_file())

    def test_a_recipe_with_default_true_reports_default_true(self):
        (self.root / 'default.md').write_text(RECIPE.replace('name: web\n', 'name: web\ndefault: true\n'))
        env = parse_environment(self.root / 'default.md', LIMITS)
        build = self.tool.build_environment(env, self.config(), self.root / 'default-out')
        self.assertIs(build['default'], True)

    def test_builds_the_template_then_the_check_output_with_a_checks_stage(self):
        self.build()
        self.assertEqual((self.root / 'targets').read_text().split(), ['template', 'check-output'])
        text = (self.root / 'rendered-template').read_text()
        self.assertTrue(text.startswith('FROM registry.example/aq/base@sha256:' + 'd' * 64 + ' AS template\n'))
        self.assertIn('RUN ["sh", "-c", "mkdir -p /tools"]', text)
        self.assertIn('FROM template AS checks\n', text)
        self.assertLess(text.index('COPY --from=aq-sandbox-runtime'), text.index('FROM template AS checks'))
        self.assertIn('(node --version\\n) > /aq-checks/0.out 2>&1; echo $? > /aq-checks/0.exit', text)
        self.assertTrue(text.endswith('FROM scratch AS check-output\nCOPY --from=checks /aq-checks /\n'))

    def test_a_recipe_without_checks_builds_once(self):
        self.env = {**self.env, 'checks': []}
        self.assertEqual(self.build()['toolchain'], [])
        self.assertEqual((self.root / 'targets').read_text().split(), ['template'])

    def test_a_failing_check_fails_the_build_at_check(self):
        self.behave(checks=[['v24.22.3\n', 0], ['pnpm: not found\n', 127]])
        log = self.fails('check')
        self.assertIn('pnpm --version', log)
        self.assertIn('pnpm: not found', log)
        self.assertIn('127', log)
        self.assertNotIn('node --version', log)

    def test_a_missing_check_result_fails_at_check(self):
        self.behave(checks=[['v24.22.3\n', 0]])
        self.fails('check')

    def test_a_buildx_failure_is_install_and_a_scan_refusal_is_scan(self):
        self.behave(fail='template', error='E: Unable to locate package nope\n')
        self.assertIn('Unable to locate package nope', self.fails('install'))
        self.assertEqual((self.root / 'targets').read_text().split(), ['template'])
        self.behave(fail='check-output', error='no space left\n')
        self.assertIn('no space left', self.fails('check', 'second'))
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]])
        fixtures.oci(self.root / 'fixture.tar', [{'home/a/.codex/auth.json': b'{}'},
                                                 {'opt/aq/manifest.json': json.dumps(fixtures.bundle()).encode()}])
        self.assertNotIn('auth.json', self.fails('scan', 'third'))

    def test_build_log_redacts_provider_keys(self):
        key = 'sk-' + '0123456789abcdefghijklmnop'
        self.behave(fail='template', error=f'step 3: curl -H "Bearer {key}"\nexport OPENAI_API_KEY=hunter2hunter2\n'
                                           'visible line\n')
        log = self.fails('install')
        self.assertNotIn('sk-0123', log)
        self.assertNotIn('hunter2', log)
        self.assertIn('[redacted]', log)
        self.assertIn('OPENAI_API_KEY=[redacted]', log)
        self.assertIn('visible line', log)

    def test_check_output_is_redacted(self):
        key = 'sk-' + '0123456789abcdefghijklmnop'
        self.behave(checks=[[f'token {key}\n', 0], ['ok\n', 0]])
        output = self.build()['toolchain'][0]['output']
        self.assertNotIn('sk-0123', output)
        self.assertIn('[redacted]', output)

    def test_build_log_is_bounded_by_limits(self):
        for limit, noise in [(1000, 'line of build output\n' * 20000), (64, 'x' * 5000 + '\n'),
                             (100, '\u00e9' * 400)]:
            with self.subTest(limit=limit):
                self.behave(fail='template', noise=noise, error='\nthe final error\n')
                log = self.fails('install', f'out{limit}', log_bytes=limit)
                self.assertLessEqual(len(log.encode()), limit)
                self.assertIn('the final error', log)

    def test_check_output_is_bounded_by_limits(self):
        self.behave(checks=[['v' * 500, 0], ['10.34.5\n', 0]])
        build = self.build(log_bytes=100)
        self.assertLessEqual(len(build['toolchain'][0]['output'].encode()), 100)

    def test_a_secret_cut_by_the_bound_does_not_leak(self):
        key = 'sk-' + 'A1b2C3d4' * 8
        self.behave(fail='template', noise='filler\n' * 5000 + key + '\n')
        self.assertNotIn('A1b2C3d4', self.fails('install', log_bytes=40))

    def test_limits_are_required_and_validated(self):
        config = self.config()
        del config['limits']
        with self.assertRaises(ValueError):
            self.tool.build_environment(self.env, config, self.root / 'x')
        with self.assertRaises(ValueError):
            self.tool.validate_config({**self.config(), 'limits': {**LIMITS, 'log_bytes': 0}})
        with self.assertRaises(ValueError):
            self.tool.validate_config({**self.config(), 'limits': {**LIMITS, 'build_wall_ms': True}})

    def test_the_build_timeout_comes_from_limits(self):
        (self.root / 'docker').write_text('#!/bin/sh\necho started >&2\nsleep 5\n')
        log = self.fails('install', build_wall_ms=200)
        self.assertIn('started', log)
        self.assertIn('timed out', log)


SKOPEO = 'registry.example/skopeo@sha256:' + 'f' * 64
TARGET = 'registry.example/aq/projects/demo'


class JobTests(Fixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.repo = self.root / 'repo'
        (self.repo / '.aq/environments').mkdir(parents=True)
        (self.repo / '.aq/environments/web.md').write_text(RECIPE)
        (self.repo / '.git').mkdir()
        (self.repo / 'package.json').write_text('{}')
        (self.root / 'builder.json').write_text(json.dumps({**fixtures.config(), 'docker_binary': str(self.root / 'docker'),
                                                            'skopeo_image': SKOPEO}))

    def job(self, kind='build', recipes=('web',), run=None, **limits):
        job = {'job_id': '0b6f2c1e-8f0a-4c4e-9a52-6a3c1b7d9e01', 'kind': kind,
               'project_id': '5d2f7e4a-1b3c-4d5e-8f60-718293a4b5c6', 'project': 'demo', 'commit': 'c' * 40,
               'recipes': list(recipes), 'run': run, 'base_template': 'registry.example/aq/base@sha256:' + 'd' * 64,
               'target_repository': TARGET, 'limits': {**LIMITS, **limits}}
        path = self.root / f'job-{kind}.json'
        path.write_text(json.dumps(job))
        return path

    def calls(self):
        path = self.root / 'calls'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def runs(self):
        return [call for call in self.calls() if call[2] == 'run']

    def run_build(self, **job):
        return self.tool.environment_build(self.job(**job), self.repo, self.root / 'builder.json', self.root / 'out')

    def run_try(self, run='pnpm test', **job):
        return self.tool.environment_try(self.job('try', run=run, **job), self.repo, self.root / 'builder.json')

    def test_build_publishes_only_after_the_scan(self):
        fixtures.oci(self.root / 'fixture.tar', [{'home/a/.codex/auth.json': b'{}'},
                                                 {'opt/aq/manifest.json': json.dumps(fixtures.bundle()).encode()}])
        result = self.run_build()
        self.assertEqual(result['outcome'], 'failed')
        self.assertEqual((result['failure']['recipe'], result['failure']['step']), ('web', 'scan'))
        self.assertEqual((result['builds'], result['run']), ([], None))
        self.assertEqual(self.runs(), [])
        self.assertFalse((self.root / 'templates').exists())

    def test_build_publishes_with_preserve_digests_and_reports_the_scanned_digest(self):
        result = self.run_build()
        self.assertEqual(list(result), ['job_id', 'outcome', 'builds', 'failure', 'run'])
        self.assertEqual((result['outcome'], result['failure'], result['run']), ('built', None, None))
        build = result['builds'][0]
        self.assertIs(build['default'], False)
        self.assertEqual(build['digest'], self.digest)
        self.assertEqual(build['image'], f"{TARGET}@{build['digest']}")
        self.assertEqual(build['toolchain'][0], {'command': 'node --version', 'output': 'v24.22.3\n'})
        calls = self.calls()
        self.assertEqual([call[2] for call in calls], ['buildx', 'buildx', 'run'])
        publish = calls[-1]
        skopeo_argv = publish[publish.index(SKOPEO) + 1:]
        archive = '/aq-publish/image.oci.tar'
        self.assertEqual(skopeo_argv[:3], ['copy', '--preserve-digests', f'oci-archive:{archive}'])
        self.assertEqual(skopeo_argv[3:], [f'docker://{TARGET}@{self.digest}'])
        self.assertIn(f"type=bind,src={self.root / 'out/web'},dst=/aq-publish,readonly", publish)
        self.assertTrue((self.root / 'out/web/image.oci.tar').is_file())
        self.assertFalse((self.root / 'templates').exists())

    def test_nothing_is_published_until_every_recipe_passes(self):
        (self.repo / '.aq/environments/api.md').write_text(RECIPE.replace('name: web', 'name: api'))
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], fail='check-output', error='boom\n')
        result = self.run_build(recipes=('api', 'web'))
        self.assertEqual((result['failure']['recipe'], result['failure']['step']), ('api', 'check'))
        self.assertEqual(self.runs(), [])

    def test_a_failed_publish_is_reported_redacted(self):
        key = 'sk-' + '0123456789abcdefghijklmnop'
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], publish_error=f'denied for {key}\n')
        result = self.run_build()
        self.assertEqual((result['outcome'], result['failure']['step']), ('failed', 'publish'))
        self.assertNotIn('sk-0123', result['failure']['log'])
        self.assertIn('denied for [redacted]', result['failure']['log'])

    def test_a_recipe_that_does_not_parse_fails_at_parse(self):
        result = self.run_build(recipes=('missing',))
        self.assertEqual(result['failure'], {'recipe': 'missing', 'step': 'parse',
                                             'log': 'the recipe file is missing or unreadable'})
        (self.repo / '.aq/environments/other.md').write_text(RECIPE)
        result = self.tool.environment_build(self.job(recipes=('other',)), self.repo, self.root / 'builder.json',
                                             self.root / 'out2')
        self.assertEqual(result['failure']['step'], 'parse')
        self.assertIn('name does not match', result['failure']['log'])
        (self.repo / '.aq/environments/bad.md').write_text('---\nname: bad\nparent: x\n---\n## Install\n- true\n')
        result = self.tool.environment_build(self.job(recipes=('bad',)), self.repo, self.root / 'builder.json',
                                             self.root / 'out3')
        self.assertEqual(result['failure']['step'], 'parse')
        self.assertEqual(self.calls(), [])

    def test_the_job_limits_apply_to_the_build(self):
        self.behave(fail='template', noise='line of build output\n' * 2000, error='\nthe final error\n')
        result = self.run_build(log_bytes=200)
        self.assertEqual(result['failure']['step'], 'install')
        self.assertLessEqual(len(result['failure']['log'].encode()), 200)
        self.assertIn('the final error', result['failure']['log'])

    def test_try_runs_the_command_in_the_built_image_and_exports_nothing(self):
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], run=['ok\n', 0])
        result = self.run_try('pnpm install --frozen-lockfile && pnpm test:e2e')
        self.assertEqual(result['run'], {'exit': 0, 'output': 'ok\n'})
        self.assertEqual(result['builds'], [])
        self.assertEqual((result['outcome'], result['failure']), ('built', None))
        docker_argv = [arg for call in self.calls() for arg in call]
        self.assertFalse(any(a.startswith('type=oci') for a in docker_argv))
        self.assertEqual(self.runs(), [])
        self.assertEqual((self.root / 'targets').read_text().split(), ['try-output'])
        self.assertEqual((self.root / 'context').read_text(), str(self.repo.resolve()))
        self.assertIn('.git', (self.root / 'ignored').read_text().split())
        text = (self.root / 'rendered-try-output').read_text()
        self.assertIn('FROM template AS run\nCOPY . /workspace\n', text)
        self.assertIn('(pnpm install --frozen-lockfile && pnpm test:e2e\\n) > /aq-run/0.out 2>&1', text)
        self.assertTrue(text.endswith('FROM scratch AS try-output\nCOPY --from=checks /aq-checks /checks\n'
                                      'COPY --from=run /aq-run /run\n'))
        self.assertEqual(sorted(p.name for p in self.repo.iterdir()), ['.aq', '.git', 'package.json'])

    def test_try_reports_a_failing_command_without_failing_the_job(self):
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], run=['1 test failed\n', 1])
        result = self.run_try()
        self.assertEqual(result['outcome'], 'built')
        self.assertEqual(result['run'], {'exit': 1, 'output': '1 test failed\n'})

    def test_try_output_is_redacted_and_bounded(self):
        key = 'sk-' + '0123456789abcdefghijklmnop'
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], run=[('x' * 500) + f'\n{key}\n', 0])
        output = self.run_try(log_bytes=100)['run']['output']
        self.assertLessEqual(len(output.encode()), 100)
        self.assertNotIn('sk-0123', output)

    def test_try_fails_at_check_install_or_run(self):
        self.behave(checks=[['v24.22.3\n', 0], ['pnpm: not found\n', 127]], run=['ok\n', 0])
        result = self.run_try()
        self.assertEqual((result['outcome'], result['failure']['step'], result['run']), ('failed', 'check', None))
        self.behave(checks=[], fail='try-output', error='E: Unable to locate package nope\n')
        self.assertEqual(self.run_try()['failure']['step'], 'install')
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]])
        self.assertEqual(self.run_try()['failure'], {'recipe': 'web', 'step': 'run',
                                                     'log': 'the command produced no result'})

    def test_try_without_a_command_runs_only_the_checks(self):
        result = self.run_try(run=None)
        self.assertEqual((result['outcome'], result['run'], result['builds']), ('built', None, []))
        self.assertEqual((self.root / 'targets').read_text().split(), ['check-output'])

    def test_jobs_are_validated(self):
        base = json.loads(self.job().read_text())
        config = json.loads((self.root / 'builder.json').read_text())
        for kind, change in [('build', {'kind': 'try'}), ('build', {'run': 'true'}), ('build', {'extra': 1}),
                             ('build', {'recipes': []}), ('build', {'recipes': ['web', 'web']}),
                             ('build', {'recipes': ['../web']}), ('build', {'commit': 'c' * 39}),
                             ('build', {'base_template': 'registry.example/aq/base:latest'}),
                             ('build', {'target_repository': 'Bad Repo'}),
                             ('build', {'limits': {k: v for k, v in LIMITS.items() if k != 'log_bytes'}}),
                             ('build', {'limits': {**LIMITS, 'build_wall_ms': 0}}),
                             ('try', {'kind': 'try', 'recipes': ['web', 'api']}),
                             ('try', {'kind': 'try', 'run': 'echo TOKEN=private-value'}),
                             ('try', {'kind': 'try', 'run': 'a\nb'})]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.tool.job_config(config, self.tool.validate_job({**base, **change}, kind))
        del config['skopeo_image']
        (self.root / 'builder.json').write_text(json.dumps(config))
        with self.assertRaisesRegex(ValueError, 'skopeo'):
            self.run_build()

    def cli(self, command):
        binary = os.environ.get('AQ_TEMPLATE_BINARY')
        prefix = [binary, 'template'] if binary else [sys.executable, '-I', str(TOOLS / 'template_tool.py')]
        return subprocess.run(prefix + command, cwd=self.root, capture_output=True)

    def check_cli(self):
        result = self.cli(['environment-build', '--job', str(self.job()), '--repo', str(self.repo),
                           '--build-config', str(self.root / 'builder.json'), '--output', str(self.root / 'out')])
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(json.loads(result.stdout)['builds'][0]['digest'], self.digest)
        self.behave(checks=[['v24.22.3\n', 0], ['10.34.5\n', 0]], run=['ok\n', 0])
        result = self.cli(['environment-try', '--job', str(self.job('try', run='pnpm test')), '--repo',
                           str(self.repo), '--build-config', str(self.root / 'builder.json')])
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(json.loads(result.stdout)['run'], {'exit': 0, 'output': 'ok\n'})
        result = self.cli(['environment-try', '--job', str(self.job()), '--repo', str(self.repo),
                           '--build-config', str(self.root / 'builder.json')])
        self.assertEqual((result.returncode, result.stdout), (2, b''))
        self.assertFalse((self.root / 'templates').exists())

    def test_cli_prints_one_job_result(self):
        self.check_cli()

    @unittest.skipUnless(os.environ.get('AQ_TEMPLATE_BINARY'), 'needs the aq-worker binary')
    def test_cli_runs_jobs_through_the_worker_binary(self):
        self.check_cli()


if __name__ == '__main__':
    unittest.main()
