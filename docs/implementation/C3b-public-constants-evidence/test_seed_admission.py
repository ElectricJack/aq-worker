"""Explicit physical qualification through the installed C3b build boundary.

Requires AQ_TEMPLATE_BINARY, C3B_BUILD_CONFIG and C3B_PHYSICAL_ROOT. The
configured BuildKit node must resolve the pinned C3a parents. Outputs remain
outside Git, and only a disposable qualification allowlist is changed.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def test_actual_seed_images_pass_unchanged_admission_boundary():
    root = Path(tempfile.mkdtemp(prefix='admission-', dir=os.environ['C3B_PHYSICAL_ROOT']))
    print('QUALIFICATION OUTPUT:', root, flush=True)
    cfg = json.loads(Path(os.environ['C3B_BUILD_CONFIG']).read_text())
    cfg['allowlist'] = 'allowlist.qualification.json'
    cfg['builder_identity'] = 'c3b-public-constants-qualification'
    config = root / 'builder.json'
    config.write_text(json.dumps(cfg))
    expected = {
        'rust': 'sha256:4c9c20007445c3a207f31787e635ec753e64b13f22300cd90030784f7862a1c5',
        'node': 'sha256:5e0e668ad48da42b5d46f03e971123bc9552c5721d14a147096a7250d2305710',
    }
    evidence = []
    for name, digest in expected.items():
        recipe = Path(__file__).with_name(name + '.recipe.json')
        command = [os.environ['AQ_TEMPLATE_BINARY'], 'template', 'build', str(recipe),
                   '--build-config', str(config), '--output', str(root / name)]
        print('COMMAND:', json.dumps(command), flush=True)
        result = subprocess.run(command, capture_output=True, timeout=1800)
        assert result.returncode == 0, result.stderr.decode()
        entry = json.loads(result.stdout)
        assert entry['name'] == name and entry['image_digest'] == digest
        assert entry['provenance']['finalize_policy'] == 'aq-finalize-v1'
        assert entry['provenance']['credential_scan_policy'] == 'aq-credential-scan-v1'
        assert entry['harness_versions'] == {'codex': '0.166.0'}
        archive = root / name / 'image.oci.tar'
        evidence.append({'name': name, 'entry': entry, 'archive': str(archive),
                         'archive_sha256': hashlib.file_digest(archive.open('rb'), 'sha256').hexdigest()})
        (root / 'admission.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print('ADMITTED:', name, digest, flush=True)
    allowlist = json.loads((root / cfg['allowlist']).read_text())
    assert allowlist['entries'] == [item['entry'] for item in evidence]
