"""Explicit local BuildKit/OS smoke; no registry publication or provider execution."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]

def module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'packaging/sandbox' / (name + '.py'))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def test_real_buildkit_oci_export_and_scan():
    testspec = importlib.util.spec_from_file_location('fixtures', ROOT / 'scripts/tests/test_template_tool.py')
    fixtures = importlib.util.module_from_spec(testspec)
    testspec.loader.exec_module(fixtures)
    with tempfile.TemporaryDirectory(prefix='c3b-physical-') as directory:
        root = Path(directory)
        context = root / 'context'
        context.mkdir()
        (context / 'manifest.json').write_text(json.dumps(fixtures.bundle()))
        (context / 'Dockerfile').write_text('FROM scratch\nCOPY manifest.json /opt/aq/manifest.json\n')
        output = root / 'image.tar'
        subprocess.run(['docker', '--context', 'default', 'buildx', 'build', '--builder', 'aq-c3b-qualification',
                        '--platform', 'linux/amd64', '--provenance=false', '--sbom=false',
                        '--output', f'type=oci,dest={output},compression=gzip,force-compression=true',
                        str(context)], check=True)
        scanned = module('credential_scan').scan_oci(output)
        assert scanned['layers_scanned'] == 1
        assert scanned['bundle_manifest']['harnesses'][0]['version'] == '0.166.0'
        print('Observed real OCI manifest digest:', scanned['image_digest'])


def test_finalize_on_existing_c3a_image():
    subprocess.run(['docker', '--context', 'default', 'run', '--rm', '--network', 'none',
                    '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                    '--entrypoint', '/bin/sh', 'aq-sandbox-c3a:after', '-ec',
                    module('template_tool').FINALIZE], check=True)
