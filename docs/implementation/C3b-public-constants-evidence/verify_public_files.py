"""Reproduce public-byte verification independently of any candidate image.

Run explicitly; this downloads pinned public archives into a disposable cache.
No matched content or private-key example bytes are printed or stored in Git.
"""
import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import urllib.request


def verify(cache):
    root = Path(__file__).resolve().parents[3]
    spec = importlib.util.spec_from_file_location('credential_scan', root / 'packaging/sandbox/credential_scan.py')
    scanner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(scanner)
    records = json.loads(Path(__file__).with_name('public-files.json').read_text())['files']
    assert scanner.PUBLIC_FILE_CONSTANTS == {r['sha256']: r['bytes'] for r in records}
    sources = {}
    for record in records:
        source = record['upstream']
        sources.setdefault((source['url'], source['archive_sha256']), []).append(record)
    cache.mkdir(parents=True, exist_ok=True)
    for (url, expected), members in sources.items():
        path = cache / expected
        if not path.exists():
            urllib.request.urlretrieve(url, path)
        assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, 'public archive digest mismatch'
        data = io.BytesIO(subprocess.check_output(['dpkg-deb', '--fsys-tarfile', str(path)])) if url.endswith('.deb') else path
        with tarfile.open(fileobj=data) if isinstance(data, io.BytesIO) else tarfile.open(data) as archive:
            for record in members:
                source = record['upstream']
                content = archive.extractfile(source['member']).read()
                if 'transformation' in source:
                    assert source['transformation'] == 'replace shebang with #!/usr/bin/python'
                    content = b'#!/usr/bin/python\n' + content.split(b'\n', 1)[1]
                assert len(content) == record['bytes']
                assert hashlib.sha256(content).hexdigest() == record['sha256'], 'public member digest mismatch'
                # These are deliberately still positives under strict matching.
                assert scanner.secret_shaped(content)
        print('verified', len(members), 'public members from', url, flush=True)
    print('verified all', len(records), 'catalog fingerprints', flush=True)


def test_public_file_fingerprints():
    verify(Path(os.environ['C3B_PUBLIC_CACHE']))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--cache', type=Path, required=True)
    verify(parser.parse_args().cache)
