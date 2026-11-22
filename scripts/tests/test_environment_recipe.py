"""Behavior tests for project environment recipe parsing."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

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


if __name__ == '__main__':
    unittest.main()
