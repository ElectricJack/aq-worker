#!/usr/bin/env python3
"""Build/check the deterministic, pinned native contract source publication."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import tarfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "protocol/aq-admission"
RELEASE = ROOT / "protocol/releases"
VERSION = "0.3.2"
NAME = f"aq-admission-{VERSION}"


def publication():
    payload = io.BytesIO()
    hashes = {}
    with tarfile.open(fileobj=payload, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for path in sorted(SOURCE.rglob("*")):
            if not path.is_file():
                continue
            relative = path.relative_to(SOURCE).as_posix()
            if relative.startswith("target/") or relative == "Cargo.lock":
                continue
            content = path.read_bytes()
            hashes[relative] = hashlib.sha256(content).hexdigest()
            entry = tarfile.TarInfo(f"{NAME}/{relative}")
            entry.size = len(content)
            entry.mode = 0o644
            entry.mtime = 0
            archive.addfile(entry, io.BytesIO(content))
    compressed = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=compressed, mtime=0) as stream:
        stream.write(payload.getvalue())
    packed = compressed.getvalue()
    manifest = {
        "package": "aq-admission", "version": VERSION,
        "owner": "aq-worker", "source_path": "protocol/aq-admission",
        "archive": f"{NAME}.tar.gz", "sha256": hashlib.sha256(packed).hexdigest(),
        "files": hashes,
    }
    return packed, (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    packed, manifest = publication()
    artifacts = {f"{NAME}.tar.gz": packed, f"{NAME}.json": manifest}
    if args.check:
        for name, expected in artifacts.items():
            if (RELEASE / name).read_bytes() != expected:
                raise SystemExit(f"publication drift: {name}")
    else:
        RELEASE.mkdir(exist_ok=True)
        for name, content in artifacts.items():
            (RELEASE / name).write_bytes(content)
    print(f"{NAME}: sha256={json.loads(manifest)['sha256']}, {len(json.loads(manifest)['files'])} files")


if __name__ == "__main__":
    main()
