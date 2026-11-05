import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class SandboxPackaging(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('sandbox_package', ROOT / 'scripts/package-sandbox.py')
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.inputs = self.root / 'inputs'
        self.inputs.mkdir()
        self.pins = json.loads((ROOT / 'packaging/sandbox/pins.json').read_text())
        for name, pin in self.pins['inputs'].items():
            content = name.encode()
            (self.inputs / name).write_bytes(content)
            pin['sha256'] = hashlib.sha256(content).hexdigest()
        (self.inputs / 'credentials.json').write_text('private sentinel')
        repo = self.root / 'repo'
        repo.mkdir()
        for path in ['Cargo.toml', 'Cargo.lock', 'crates/aq-worker/Cargo.toml',
                     'crates/aq-session-host/Cargo.toml', 'crates/aq-agent-client/Cargo.toml']:
            target = repo / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / path, target)
        shutil.copytree(ROOT / 'packaging/sandbox', repo / 'packaging/sandbox')
        (repo / 'vendor').mkdir()
        (repo / 'vendor/public.txt').write_text('public dependency')
        (repo / 'protocol/aq-admission/src').mkdir(parents=True)
        (repo / 'protocol/aq-admission/src/lib.rs').write_text('// committed workspace member\n')
        (repo / 'protocol/unrelated-private.txt').write_text('private sentinel')
        subprocess.run(['git', 'init', '-q', str(repo)], check=True)
        subprocess.run(['git', 'add', '.'], cwd=repo, check=True)
        subprocess.run(['git', 'commit', '-qm', 'fixture'], cwd=repo, check=True)
        (repo / 'crates/private-untracked.txt').write_text('private sentinel')
        self.module.ROOT = repo
        self.revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()

    def package(self, destination, pins=None):
        return self.module.package(self.revision, self.inputs, destination, pins=pins or self.pins)

    def test_corrupt_input_refuses(self):
        (self.inputs / 'codex').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch: codex'):
            self.package(self.root / 'out')
        self.assertFalse((self.root / 'out').exists())

    def test_claude_input_is_pinned_and_checked(self):
        output = self.root / 'out'
        self.package(output)
        sums = (output / 'runtime/SHA256SUMS').read_text().splitlines()
        self.assertIn(self.pins['inputs']['claude']['sha256'] + '  claude', sums)
        self.assertEqual((output / 'runtime/claude').read_bytes(), b'claude')
        (self.inputs / 'claude').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch: claude'):
            self.package(self.root / 'corrupt')
        self.assertFalse((self.root / 'corrupt').exists())

    def test_opencode_input_is_pinned_and_checked(self):
        output = self.root / 'out'
        self.package(output)
        sums = (output / 'runtime/SHA256SUMS').read_text().splitlines()
        self.assertIn(self.pins['inputs']['opencode']['sha256'] + '  opencode', sums)
        self.assertEqual((output / 'runtime/opencode').read_bytes(), b'opencode')
        (self.inputs / 'opencode').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch: opencode'):
            self.package(self.root / 'corrupt')
        self.assertFalse((self.root / 'corrupt').exists())

    def test_gemini_input_is_pinned_and_checked(self):
        output = self.root / 'out'
        self.package(output)
        sums = (output / 'runtime/SHA256SUMS').read_text().splitlines()
        self.assertIn(self.pins['inputs']['gemini.tgz']['sha256'] + '  gemini.tgz', sums)
        self.assertEqual((output / 'runtime/gemini.tgz').read_bytes(), b'gemini.tgz')
        (self.inputs / 'gemini.tgz').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch: gemini.tgz'):
            self.package(self.root / 'corrupt')
        self.assertFalse((self.root / 'corrupt').exists())

    def test_symlink_input_refuses(self):
        (self.inputs / 'codex').unlink()
        (self.inputs / 'codex').symlink_to('aq-git')
        with self.assertRaisesRegex(ValueError, 'regular input'):
            self.package(self.root / 'out')

    def test_context_excludes_private_inputs_and_untracked_worktree(self):
        output = self.root / 'out'
        self.package(output)
        self.assertNotIn('credentials.json', [p.name for p in output.rglob('*')])
        with tarfile.open(output / 'runtime/worker-source.tar') as archive:
            names = archive.getnames()
        self.assertIn('Cargo.lock', names)
        embedded = {'packaging/sandbox/template_tool.py', 'packaging/sandbox/credential_scan.py'}
        self.assertTrue(embedded <= set(names), 'runtime source must include embedded template tools')
        self.assertIn('protocol/aq-admission/src/lib.rs', names)
        self.assertNotIn('protocol/unrelated-private.txt', names)
        self.assertTrue(all(n.startswith(('crates/', 'vendor/', 'protocol/aq-admission/')) or n in
                            {'crates', 'vendor', 'Cargo.lock', 'Cargo.toml', 'packaging', 'packaging/sandbox',
                             'protocol', 'protocol/aq-admission'} | embedded for n in names))
        self.assertNotIn('crates/private-untracked.txt', names)
        self.assertFalse(any(n.startswith(('.aq/', '.git/', 'secrets/')) for n in names))

    def test_harness_bump_leaves_os_context_identical(self):
        before = self.root / 'before'
        after = self.root / 'after'
        self.package(before)
        self.pins['inputs']['codex']['version'] = '0.167.0'
        (self.inputs / 'codex').write_bytes(b'new harness')
        self.pins['inputs']['codex']['sha256'] = hashlib.sha256(b'new harness').hexdigest()
        self.package(after)
        def files(root):
            return {str(p.relative_to(root)): p.read_bytes() for p in root.rglob('*') if p.is_file()}
        self.assertEqual(files(before / 'os'), files(after / 'os'))
        self.assertNotEqual(files(before / 'runtime'), files(after / 'runtime'))

    def test_floating_image_refuses(self):
        self.pins['base'] = 'debian:bookworm-slim'
        with self.assertRaisesRegex(ValueError, 'digest-pinned'):
            self.package(self.root / 'out')

    def test_existing_destination_refuses(self):
        output = self.root / 'out'
        output.mkdir()
        with self.assertRaisesRegex(ValueError, 'already exists'):
            self.package(output)


if __name__ == '__main__':
    unittest.main()
