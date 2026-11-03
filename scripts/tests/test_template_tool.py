"""Behavior tests for template admission; OCI inputs contain real tar layers."""
import hashlib
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
TOOLS = ROOT / 'packaging/sandbox'


def load(name):
    path = TOOLS / (name + '.py')
    assert path.is_file(), 'template build/scanner tool must exist'
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def layer(files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode='w') as archive:
        for name, value in files.items():
            entry = tarfile.TarInfo(name)
            if isinstance(value, tuple):
                entry.type = tarfile.SYMTYPE
                entry.linkname = value[0]
                archive.addfile(entry)
            else:
                entry.size = len(value)
                archive.addfile(entry, io.BytesIO(value))
    return stream.getvalue()


def bundle():
    return {'schema_version': 1, 'platform': 'linux/amd64',
            'source': {'revision': 'a' * 40, 'archive_sha256': 'b' * 64},
            'runner': artifact('aq-worker', '0.5.1', 'bin/aq-worker'),
            'session_host': artifact('aq-session-host', '0.1.5', 'bin/aq-session-host'),
            'aq_git': artifact('aq-git', '0.2.0', 'bin/aq-git'),
            'node': artifact('node', '22.16.0', 'node/bin/node'),
            'harnesses': [artifact('codex', '0.166.0', 'harnesses/codex/codex')],
            'proxies': {'responses': 'in-process:aq-worker', 'aq_loopback': 'in-process:aq-worker'}}


def artifact(name, version, path):
    return {'name': name, 'version': version, 'path': '/opt/aq/' + path,
            'sha256': 'c' * 64, 'bytes': 4}


def oci(path, layers=None, env=None, corrupt=False, compressed=False, media=None):
    blobs = {}

    def blob(value, media):
        content = value if isinstance(value, bytes) else json.dumps(value).encode()
        digest = hashlib.sha256(content).hexdigest()
        blobs['blobs/sha256/' + digest] = content
        return {'mediaType': media, 'digest': 'sha256:' + digest, 'size': len(content)}

    if layers is None:
        layers = [{'opt/aq/manifest.json': json.dumps(bundle()).encode()}]
    raw_layers = [layer(files) for files in layers]
    descriptors = [blob(gzip.compress(raw) if compressed else raw,
                        media or ('application/vnd.oci.image.layer.v1.tar' + ('+gzip' if compressed else '')))
                   for raw in raw_layers]
    config = blob({'architecture': 'amd64', 'os': 'linux', 'config': {'Env': env or []},
                   'rootfs': {'type': 'layers', 'diff_ids': ['sha256:' + hashlib.sha256(raw).hexdigest() for raw in raw_layers]}},
                  'application/vnd.oci.image.config.v1+json')
    manifest = blob({'schemaVersion': 2, 'mediaType': 'application/vnd.oci.image.manifest.v1+json',
                     'config': config, 'layers': descriptors}, 'application/vnd.oci.image.manifest.v1+json')
    index = {'schemaVersion': 2, 'manifests': [manifest]}
    if corrupt:
        blobs[config['digest'].replace('sha256:', 'blobs/sha256/')] = b'{}'
    with tarfile.open(path, 'w') as archive:
        for name, value in {'oci-layout': b'{"imageLayoutVersion":"1.0.0"}',
                            'index.json': json.dumps(index).encode(), **blobs}.items():
            item = tarfile.TarInfo(name)
            item.size = len(value)
            archive.addfile(item, io.BytesIO(value))
    return manifest['digest']


def recipe():
    return {'schema_version': 1, 'name': 'test-tools', 'version': '1',
            'parent': 'aq-sandbox-os@sha256:' + 'a' * 64,
            'install_steps': [['sh', '-c', 'mkdir -p /tools']]}


def config():
    return {'schema_version': 1, 'role': 'template-build', 'docker_context': 'build-desktop',
            'buildkit_builder': 'aq-templates', 'builder_identity': 'desktop-1',
            'os_image': 'aq-sandbox-os@sha256:' + 'a' * 64,
            'runtime_image': 'aq-sandbox-runtime@sha256:' + 'b' * 64,
            'target_repository': 'registry.example/aq/template', 'registry_visibility': 'private',
            'allowlist': 'templates/allowlist.json'}


class TemplateTests(unittest.TestCase):
    def test_finalize_refuses_template_missing_tmux_or_users(self):
        tool = load('template_tool')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / 'manifest.json'
            manifest.write_text('{}')
            # A controlled PATH exercises the actual fixed shell, with no Docker dependency.
            for name in ['getent', 'tmux', 'bwrap', 'git']:
                (root / name).write_text('#!/bin/sh\nexit 0\n')
                (root / name).chmod(0o755)
            script = tool.FINALIZE.replace('/opt/aq/manifest.json', str(manifest))
            def check():
                return subprocess.run(['/bin/sh', '-ec', script], env={'PATH': directory},
                                      capture_output=True).returncode
            self.assertEqual(check(), 0)
            (root / 'tmux').unlink()
            self.assertNotEqual(check(), 0)
            (root / 'tmux').write_text('#!/bin/sh\nexit 0\n')
            (root / 'tmux').chmod(0o755)
            for missing in ['runner', 'agent']:
                (root / 'getent').write_text(f'#!/bin/sh\n[ "$2" != "{missing}" ]\n')
                self.assertNotEqual(check(), 0)
            (root / 'getent').write_text('#!/bin/sh\nexit 0\n')
            manifest.unlink()
            self.assertNotEqual(check(), 0)

    def test_credential_scan_fails_build_with_auth_json(self):
        scanner = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            oci(archive, [{'home/agent/.codex/auth.json': b'{}'},
                          {'home/agent/.codex/.wh.auth.json': b'',
                           'opt/aq/manifest.json': json.dumps(bundle()).encode()}])
            with self.assertRaisesRegex(ValueError, 'credential'):
                scanner.scan_oci(archive)

    def test_scanner_rejects_paths_env_content_links_and_blob_substitution(self):
        scanner = load('credential_scan')
        bad_paths = ['custom-codex/auth.json', 'home/a/.claude/.credentials.json',
                     'home/a/.gemini/oauth_creds.json', 'home/a/.git-credentials',
                     'root/.netrc', 'root/.ssh/id_ed25519', 'root/.docker/config.json',
                     'root/.aws/credentials', 'root/.config/gcloud/credentials.db',
                     'root/.azure/accessTokens.json', 'root/.config/gh/hosts.yml']
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            for name in bad_paths:
                with self.subTest(name=name):
                    oci(archive, [{name: b'{}'}])
                    with self.assertRaisesRegex(ValueError, 'credential'):
                        scanner.scan_oci(archive)
            for env in ['OPENAI_API_KEY=', 'ANTHROPIC_API_KEY=example', 'AWS_SESSION_TOKEN=value',
                        'GOOGLE_APPLICATION_CREDENTIALS=/custom/file']:
                oci(archive, env=[env])
                with self.assertRaisesRegex(ValueError, 'credential'):
                    scanner.scan_oci(archive)
            for data in [b'-----BEGIN OPENSSH PRIVATE KEY-----', b'sk-proj-' + b'x' * 40,
                         b'GEMINI_API_KEY=some-value']:
                oci(archive, [{'tmp/random': data}])
                with self.assertRaisesRegex(ValueError, 'credential'):
                    scanner.scan_oci(archive)
            oci(archive, [{'tmp/link': ('/root/.ssh/id_rsa',)}])
            with self.assertRaisesRegex(ValueError, 'credential'):
                scanner.scan_oci(archive)
            oci(archive, corrupt=True)
            with self.assertRaisesRegex(ValueError, 'descriptor|digest'):
                scanner.scan_oci(archive)
            oci(archive, [{'../../escape': b''}])
            with self.assertRaises(ValueError):
                scanner.scan_oci(archive)

    def test_clean_scan_binds_oci_manifest_and_manifest_whiteout_refuses(self):
        scanner = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            digest = oci(archive)
            result = scanner.scan_oci(archive)
            self.assertEqual(result['image_digest'], digest)
            self.assertEqual(result['bundle_manifest']['harnesses'][0]['version'], '0.166.0')
            oci(archive, [{'opt/aq/manifest.json': json.dumps(bundle()).encode()},
                          {'opt/aq/.wh.manifest.json': b''}])
            with self.assertRaisesRegex(ValueError, 'manifest'):
                scanner.scan_oci(archive)

    def test_gzip_oci_layers_and_unsupported_compression(self):
        scanner = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            digest = oci(archive, compressed=True)
            self.assertEqual(scanner.scan_oci(archive)['image_digest'], digest)
            oci(archive, media='application/vnd.oci.image.layer.v1.tar+zstd')
            with self.assertRaisesRegex(ValueError, 'compression'):
                scanner.scan_oci(archive)

    def test_credential_directory_aliases_cannot_hide_auth_files(self):
        scanner = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            for directory_name, credential_name in [('.docker', 'config.json'),
                                                     ('.ssh', 'id_rsa'),
                                                     ('.config', 'gcloud/access_tokens.db')]:
                with self.subTest(directory=directory_name):
                    oci(archive, [{'root/' + directory_name: ('/creds',)},
                                  {'creds/' + credential_name: b'{"auths":{"example":{"auth":"dXNlcjpwYXNz"}}}',
                                   'opt/aq/manifest.json': json.dumps(bundle()).encode()}])
                    with self.assertRaisesRegex(ValueError, 'credential'):
                        scanner.scan_oci(archive)

    def test_outer_oci_directory_collision_refuses(self):
        scanner = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            oci(archive)
            with tarfile.open(archive, 'a') as tar:
                duplicate = tarfile.TarInfo('./index.json/')
                duplicate.type = tarfile.DIRTYPE
                tar.addfile(duplicate)
            with self.assertRaisesRegex(ValueError, 'duplicate'):
                scanner.scan_oci(archive)

    def test_nonstandard_secret_shapes_refuse_recipe_and_image(self):
        scanner = load('credential_scan')
        tool = load('template_tool')
        # These catch credentials supplied through URL userinfo, JWTs or lowercase fields.
        secrets = ['https://username:password@example.com/private',
                   'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhYmMifQ.abcdefghijklmnopqrstuv',
                   'api_key=some-private-value', '"api_key":"some-private-value"',
                   'AWS_SECRET_ACCESS_KEY=opaque-token-value', 'ANTHROPIC_AUTH_TOKEN=opaque-token-value', 'GITHUB_TOKEN=opaque-token-value',
                   'Authorization: Bearer opaque-token-value', 'Authorization: Basic dXNlcjpwYXNz']
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'image.tar'
            for secret in secrets:
                with self.subTest(secret=secret[:12]):
                    r = recipe()
                    r['install_steps'] = [['echo', secret]]
                    with self.assertRaises(ValueError):
                        tool.validate_recipe(r, config())
                    oci(archive, [{'random/file': secret.encode()}])
                    with self.assertRaisesRegex(ValueError, 'credential'):
                        scanner.scan_oci(archive)

    def test_credential_metadata_labels_are_not_secret_assignments(self):
        scanner = load('credential_scan')
        for label in ['api_key_model_discovery=enabled', 'cli_auth_credentials_store=file',
                      'OPENAI_API_KEY\":Codex API key byte format',
                      'OPENAI_API_KEY\":\"scoped Codex auth encoding', 'has_username_or_password://',
                      '"credentials":"private-codex-chatgpt-auth-json"']:
            self.assertFalse(scanner.secret_shaped(label))
        for credential in ['"credentials":"opaque-private-value"',
                           '"credentials":"private-codex-chatgpt-auth-json-secret"',
                           'credentials=private-codex-chatgpt-auth-json']:
            self.assertTrue(scanner.secret_shaped(credential))

    def test_compiled_capability_metadata_is_not_a_provider_token(self):
        scanner = load('credential_scan')
        # Serde concatenates these public labels in the worker's .rodata.
        metadata = 'persistent-disk-during-attemptbackend_idstruct SandboxIdentity'
        self.assertFalse(scanner.secret_shaped(metadata))
        for value in [metadata + ' sk-proj-' + 'x' * 40,
                      'TOKEN=' + metadata,
                      'https://' + metadata.replace(' ', '-') + ':password@example.com',
                      'sk-during-attemptbackend_idstruct' + 'x' * 40,
                      metadata.replace('backend_idstruct ', 'backend_idstruct-secret')]:
            self.assertTrue(scanner.secret_shaped(value))

    def test_public_constants_require_exact_reviewed_content(self):
        scanner = load('credential_scan')
        # A miniature public source example stands in for a reviewed upstream
        # file. Production fingerprints are verified against public archives.
        public = b'password = "public-example"\n'
        fingerprint = hashlib.sha256(public).hexdigest()
        self.assertTrue(scanner.secret_shaped(public))
        with patch.dict(scanner.PUBLIC_FILE_CONSTANTS, {fingerprint: len(public)}, clear=True):
            self.assertIsNone(scanner.scan_content(io.BytesIO(public)))
            for changed in [public * 2, public[:-1], public + b'sk-proj-' + b'x' * 40,
                            public.replace(b'public-example', b'actual-secrets!'),
                            b'-----BEGIN OPENSSH PRIVATE KEY-----']:
                with self.assertRaisesRegex(ValueError, 'credential'):
                    scanner.scan_content(io.BytesIO(changed))
            # Captured bundle/config-style metadata never gains an exception.
            with self.assertRaisesRegex(ValueError, 'credential'):
                scanner.scan_content(io.BytesIO(public), capture=True)

    def test_public_constants_do_not_bypass_paths_or_deleted_layer_checks(self):
        scanner = load('credential_scan')
        public = b'password = "public-example"\n'
        fingerprint = hashlib.sha256(public).hexdigest()
        with patch.dict(scanner.PUBLIC_FILE_CONSTANTS, {fingerprint: len(public)}, clear=True):
            with tempfile.TemporaryDirectory() as directory:
                archive = Path(directory) / 'image.tar'
                manifest = {'opt/aq/manifest.json': json.dumps(bundle()).encode()}
                oci(archive, [{'usr/lib/public-file': public}, manifest])
                self.assertEqual(scanner.scan_oci(archive)['layers_scanned'], 2)
                for files in [{'root/.codex/auth.json': public},
                              {'usr/lib/public-file': public + b'\nTOKEN=private-value'}]:
                    oci(archive, [files, {'usr/lib/.wh.public-file': b'', **manifest}])
                    with self.assertRaisesRegex(ValueError, 'credential'):
                        scanner.scan_oci(archive)

    def test_public_constants_and_credentials_across_stream_boundaries(self):
        scanner = load('credential_scan')
        public = b'public-prefix\0-----BEGIN PRIVATE KEY-----\0public-suffix'
        fingerprint = hashlib.sha256(public).hexdigest()
        class Chunks(io.BytesIO):
            def read(self, size):
                return super().read(min(size, 7))
        with patch.dict(scanner.PUBLIC_FILE_CONSTANTS, {fingerprint: len(public)}, clear=True):
            self.assertIsNone(scanner.scan_content(Chunks(public)))
            for value in [public + b'\0ghp_' + b'x' * 40,
                          b'padding' + public,
                          public.replace(b'public-suffix', b'actual-secret')]:
                with self.assertRaisesRegex(ValueError, 'credential'):
                    scanner.scan_content(Chunks(value))

    def test_recipe_bounds_secrets_public_prefetch_and_parent_policy(self):
        tool = load('template_tool')
        cfg = config()
        tool.validate_recipe(recipe(), cfg)
        for key, value in [('name', ''), ('name', 'x' * 65), ('version', '1\nFROM evil'),
                           ('install_steps', [['echo', 'sk-proj-' + 'x' * 40]]),
                           ('install_steps', [['sh', '-c', 'echo okay\nFROM evil']]),
                           ('install_steps', [['echo', 'OPENAI_API_KEY=example']]),
                           ('install_steps', [['echo', 'x' * 4097]]),
                           ('parent', 'arbitrary@sha256:' + 'c' * 64),
                           ('extra', 'ignored')]:
            with self.subTest(key=key, value=str(value)[:30]):
                r = recipe()
                r[key] = value
                with self.assertRaises(ValueError):
                    tool.validate_recipe(r, cfg)
        r = recipe()
        r['prefetch'] = [{'repository': 'https://github.com/example/repo.git', 'commit': 'a' * 40}]
        tool.validate_recipe(r, cfg)
        with self.assertRaises(ValueError):
            tool.validate_recipe(r, {**cfg, 'registry_visibility': 'public'})
        r['prefetch'][0]['repository'] = 'https://user:password@example.com/repo.git'
        with self.assertRaises(ValueError):
            tool.validate_recipe(r, cfg)

class BuildTests(unittest.TestCase):
    def setup_build(self, root, credential=False):
        archive = root / 'fixture.tar'
        layers = [{'opt/aq/manifest.json': json.dumps(bundle()).encode()}]
        if credential:
            layers.insert(0, {'home/a/.codex/auth.json': b'{}'})
        digest = oci(archive, layers)
        docker = root / 'docker'
        docker.write_text('#!/usr/bin/python3\nimport json, pathlib, shutil, sys\n'
                          'args = sys.argv[1:]\n'
                          'assert args[:6] == ["--context", "build-desktop", "buildx", "build", "--builder", "aq-templates"]\n'
                          'assert "--provenance=false" in args and "--sbom=false" in args\n'
                          'assert args[args.index("--platform")+1] == "linux/amd64"\n'
                          'assert "OPENAI_API_KEY" not in __import__("os").environ\n'
                          'context = pathlib.Path(args[-1])\n'
                          'assert sorted(p.name for p in context.iterdir()) == ["Dockerfile"]\n'
                          f'pathlib.Path({str(root / "rendered")!r}).write_text((context / "Dockerfile").read_text())\n'
                          'output = args[args.index("--output")+1]\n'
                          'dest = output.split("dest=",1)[1].split(",",1)[0]\n'
                          f'shutil.copyfile({str(archive)!r}, dest)\n')
        docker.chmod(0o755)
        cfg = {**config(), 'docker_binary': str(docker)}
        (root / 'builder.json').write_text(json.dumps(cfg))
        (root / 'recipe.json').write_text(json.dumps(recipe()))
        return digest

    def test_build_gates_allowlist_and_emits_bound_provenance(self):
        tool = load('template_tool')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            digest = self.setup_build(root)
            result = tool.build(root / 'recipe.json', root / 'builder.json', root / 'output')
            self.assertEqual(result['image_digest'], digest)
            entry = json.loads((root / 'templates/allowlist.json').read_text())['entries'][0]
            self.assertEqual(entry, result)
            self.assertEqual(entry['image_reference'], 'registry.example/aq/template@' + digest)
            provenance = entry['provenance']
            self.assertEqual(provenance['image_digest'], digest)
            self.assertEqual(json.loads((root / 'output/provenance.json').read_text())['image_digest'], digest)
            self.assertEqual(provenance['base_digest'], 'sha256:' + 'a' * 64)
            self.assertEqual(provenance['runtime_bundle_digest'], 'sha256:' + 'b' * 64)
            self.assertEqual(provenance['builder_identity'], 'desktop-1')
            self.assertEqual(provenance['harness_versions'], {'codex': '0.166.0'})
            self.assertEqual(entry['harness_versions'], {'codex': '0.166.0'})
            self.assertRegex(provenance['recipe_digest'], '^sha256:[0-9a-f]{64}$')
            self.assertIn('T', provenance['built_at'])
            rendered = (root / 'rendered').read_text()
            self.assertIn('COPY --from=aq-sandbox-runtime@sha256:' + 'b' * 64 + ' /opt/aq /opt/aq\nRUN ', rendered)
            self.assertLess(rendered.index('mkdir -p /tools'), rendered.index('COPY --from='))
            before = (root / 'templates/allowlist.json').read_bytes()
            with self.assertRaises(ValueError):
                tool.build(root / 'recipe.json', root / 'builder.json', root / 'again')
            self.assertEqual((root / 'templates/allowlist.json').read_bytes(), before)
            derived = recipe()
            derived['parent'] = 'test-tools@1'
            self.assertEqual(tool.parent_image(derived, config(), [entry]), entry['image_reference'])

    def test_failed_scan_and_build_never_publish_allowlist(self):
        tool = load('template_tool')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.setup_build(root, credential=True)
            for failure in ['scan', 'builder']:
                if failure == 'builder':
                    (root / 'docker').write_text('#!/bin/sh\nexit 1\n')
                with self.assertRaises(ValueError):
                    tool.build(root / 'recipe.json', root / 'builder.json', root / failure)
                self.assertFalse((root / 'templates/allowlist.json').exists())

    def test_modified_public_file_never_changes_existing_allowlist(self):
        tool = load('template_tool')
        public = b'password = "public-example"\n'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.setup_build(root)
            manifest = {'opt/aq/manifest.json': json.dumps(bundle()).encode()}
            oci(root / 'fixture.tar', [{'usr/lib/public-file': public}, manifest])
            with patch.dict(tool.scan.PUBLIC_FILE_CONSTANTS,
                            {hashlib.sha256(public).hexdigest(): len(public)}, clear=True):
                tool.build(root / 'recipe.json', root / 'builder.json', root / 'clean')
                before = (root / 'templates/allowlist.json').read_bytes()
                updated = recipe()
                updated['version'] = '2'
                (root / 'recipe.json').write_text(json.dumps(updated))
                oci(root / 'fixture.tar', [{'usr/lib/public-file': public + b'TOKEN=private-value'}, manifest])
                with self.assertRaisesRegex(ValueError, 'credential'):
                    tool.build(root / 'recipe.json', root / 'builder.json', root / 'refused')
                self.assertEqual((root / 'templates/allowlist.json').read_bytes(), before)

    def test_build_config_refuses_attempt_role_floating_pins_and_unknown_fields(self):
        tool = load('template_tool')
        for change in [{'role': 'attempt'}, {'os_image': 'aq-sandbox-os:latest'},
                       {'runtime_image': 'aq-sandbox-runtime:latest'}, {'arbitrary': 'value'},
                       {'builder_identity': ''}, {'docker_context': '-bad'},
                       {'allowlist': '../outside.json'}, {'registry_visibility': 'maybe'}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                tool.validate_config({**config(), **change})

    def test_cli_build_uses_embedded_tools_without_source_tree(self):
        binary = os.environ['AQ_TEMPLATE_BINARY']
        # The scanner itself is embedded in runtime bytes; its patterns must
        # not classify their own source representation as a credential.
        with open(binary, 'rb') as stream:
            load('credential_scan').scan_content(stream)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            digest = self.setup_build(root)
            result = subprocess.run([binary, 'template', 'build', str(root / 'recipe.json'),
                                     '--build-config', str(root / 'builder.json'),
                                     '--output', str(root / 'output')], cwd=root, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(json.loads(result.stdout)['image_digest'], digest)
            result = subprocess.run([binary, 'template', 'scan', str(root / 'fixture.tar')],
                                    cwd=root, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(json.loads(result.stdout)['layers_scanned'], 1)


if __name__ == '__main__':
    unittest.main()
