"""Check the real committed runtime source archive is a standalone Cargo workspace."""
import importlib.util
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[3]


def test_exported_workspace_resolves_without_sibling_paths():
    spec = importlib.util.spec_from_file_location('package', ROOT / 'scripts/package-sandbox.py')
    package = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(package)
    with tempfile.TemporaryDirectory(prefix='c3b-source-') as directory:
        root = Path(directory)
        output = root / 'contexts'
        inputs = root / 'inputs'
        inputs.mkdir()
        pins = json.loads((ROOT / 'packaging/sandbox/pins.json').read_text())
        for name, pin in pins['inputs'].items():
            content = name.encode()
            (inputs / name).write_bytes(content)
            pin['sha256'] = hashlib.sha256(content).hexdigest()
        package.package('HEAD', inputs, output, pins=pins)
        source = root / 'source'
        source.mkdir()
        with tarfile.open(output / 'runtime/worker-source.tar') as archive:
            archive.extractall(source, filter='data')
        result = subprocess.run(['cargo', 'metadata', '--locked', '--offline', '--no-deps',
                                 '--format-version=1', '--manifest-path', str(source / 'Cargo.toml')],
                                check=True, capture_output=True)
        metadata = json.loads(result.stdout)
        manifests = [p['manifest_path'] for p in metadata['packages']]
        assert str(source / 'protocol/aq-admission/Cargo.toml') in manifests
        assert (source / 'packaging/sandbox/template_tool.py').is_file()
        assert (source / 'packaging/sandbox/credential_scan.py').is_file()
        assert all(str(source) in manifest for manifest in manifests)
        print('Exported standalone workspace members:', len(metadata['workspace_members']))
