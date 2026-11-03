"""Real tar snapshot scanning and admission ordering; no provider required."""
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from test_template_tool import load, layer, oci


class BoatMaterializationTests(unittest.TestCase):
    def test_snapshot_rejects_credentials_paths_links_and_content(self):
        scan = load('credential_scan')
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'snapshot.tar'
            for files in [
                {'root/.docker/config.json': b'{}'},
                {'home/user/.config/ascii/boat/config.json':b'{}'},
                {'home/user/.codex/auth.json': b'{}'},
                {'etc/custom': b'GITHUB_TOKEN=forbidden-value'},
                {'opt/alias': ('../../root/.ssh/id_ed25519',)},
                {'__dockervol__/work/auth.json': b'{}'},
                {'../escape': b'innocent'},
            ]:
                with self.subTest(files=list(files)):
                    archive.write_bytes(layer(files))
                    with self.assertRaises(ValueError):
                        scan.scan_snapshot(archive)
            archive.write_bytes(layer({'opt/fixture': b'clean', '__dockervol__/work/result': b'complete'}))
            result = scan.scan_snapshot(archive)
            self.assertEqual(result['files_scanned'], 2)
            self.assertTrue(result['archive_digest'].startswith('sha256:'))

    def test_public_serde_labels_do_not_hide_adjacent_credentials(self):
        scan=load('credential_scan')
        label=b'vm-isolationnested-dockerpersistent-disk-during-attemptstruct SandboxIdentitybackend_idstruct SandboxIdentity with 3 elements'
        self.assertFalse(scan.secret_shaped(label))
        for changed in [label+b' sk-'+b'x'*32,label.replace(b'persistent-disk-during-attempt',b'persistent-disk-during_attempt')]:
            self.assertTrue(scan.secret_shaped(changed))

    def test_materialization_validates_digest_and_publishes_only_after_scan(self):
        tool = load('template_tool')
        boat = load('boat_materialize')
        # Existing C3b fixtures generate a correctly scanned allowlist entry.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            image = root / 'image.tar'
            image_digest = oci(image)
            entry = {
                'name': 'fixture', 'version': '1', 'image_digest': image_digest,
                'image_reference': 'registry.example/fixture@' + image_digest,
                'harness_versions': {'codex': '1'}, 'materializations': {},
                'provenance': {'image_digest': image_digest, 'recipe_digest': 'sha256:'+'a'*64,
                    'base_digest': 'sha256:'+'b'*64, 'runtime_bundle_digest': 'sha256:'+'c'*64,
                    'harness_versions': {'codex': '1'}, 'built_at':'2026-11-02T00:00:00+00:00',
                    'builder_identity':'fixture', 'finalize_policy':'aq-finalize-v1',
                    'credential_scan_policy':'aq-credential-scan-v1'}}
            allowlist = root / 'allowlist.json'
            original = json.dumps({'schema_version':1,'entries':[entry]}).encode()
            allowlist.write_bytes(original)
            cfg = {'schema_version':1,'role':'boat-template-build','api_url':'http://127.0.0.1:1/api/v1',
                   'build_org':'team_builds','attempt_org':'team_attempts','key_file':str(root/'key'),
                   'builder_identity':'fixture','allowlist':'allowlist.json'}
            (root/'config.json').write_text(json.dumps(cfg))
            (root/'key').write_text('fixture-build-key')
            (root/'key').chmod(0o600)
            class Client:
                commands = []
                cleaned = False
                snapshot = layer({'opt/fixture': b'clean'})
                def __init__(self, *args): pass
                def json(self, method, path, body=None):
                    if method == 'DELETE':
                        self.cleaned = True
                        return {'ok':True}
                    if path == '/sandboxes' and method == 'POST':
                        self.commands.append(body)
                        assert body['noEnv'] and body['ttlSeconds'] > 0
                        return {'ok':True,'sandbox':{'id':'bx_build'}}
                    if path == '/sandboxes/bx_build/commands':
                        self.commands.append(body['command'])
                        return {'ok':True,'exitCode':0,'stdout':entry['image_reference']+'\n'}
                    if path == '/saved-snapshots' and method == 'POST':
                        return {'ok':True}
                    if path.startswith('/saved-snapshots/'):
                        return {'ok':True,'snapshot':{'status':'ready','snapshotId':'ss_immutable'}}
                    return {'ok':True,'sandbox':{'state':'ready'}}
                def download(self, path, dest): dest.write_bytes(self.snapshot)
            for bad in [True, False]:
                Client.snapshot = layer({'root/.codex/auth.json':b'{}'}) if bad else layer({'opt/fixture': b'clean'})
                with patch.object(boat, 'Client', Client):
                    if bad:
                        with self.assertRaises(ValueError):
                            boat.materialize(root/'config.json', 'fixture@1', image, root/'scan-bad')
                        self.assertEqual(allowlist.read_bytes(), original)
                    else:
                        result = boat.materialize(root/'config.json', 'fixture@1', image, root/'scan-good')
                        self.assertEqual(result['image_digest'], image_digest)
                        self.assertEqual(result['snapshot_id'],'ss_immutable')
                        tool.read_allowlist(allowlist)
            published = tool.read_allowlist(allowlist)['entries'][0]
            published['materializations']['boat']['image_digest'] = 'sha256:'+'0'*64
            with self.assertRaises(ValueError): tool.validate_entry(published)
            before=allowlist.read_bytes()
            wrong=root/'wrong-image.tar'
            from test_template_tool import bundle
            oci(wrong,layers=[{'opt/aq/manifest.json':json.dumps(bundle()).encode(),'opt/change':b'changed'}])
            with self.assertRaisesRegex(ValueError,'digest mismatch'):
                boat.materialize(root/'config.json','fixture@1',wrong,root/'must-not-publish')
            self.assertEqual(allowlist.read_bytes(),before)
            self.assertFalse((root/'must-not-publish').exists())
            self.assertTrue(any('docker pull '+entry['image_reference'] in c for c in Client.commands if isinstance(c,str)))
            self.assertTrue(any('docker create --name aq-template-keeper' in c for c in Client.commands if isinstance(c,str)))
