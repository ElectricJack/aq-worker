#!/usr/bin/env python3
"""Publish/check a deterministic self-contained aq-session-host source release.

The workspace source closure retains Cargo.lock and every path dependency.
No sibling checkout, mutable Git branch, registry AQ package or credentials are
needed to build the helper or consume its library from this archive.
"""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
RELEASE = ROOT / "protocol/releases"


def publication():
    version = tomllib.loads((ROOT / "crates/aq-session-host/Cargo.toml").read_text())["package"]["version"]
    name = f"aq-session-host-{version}-source"
    paths = subprocess.check_output([
        "git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--",
        "Cargo.toml", "Cargo.lock", "crates", "vendor", "protocol/aq-admission",
    ], cwd=ROOT).decode().split("\0")
    hashes = {}
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for relative in sorted(set(filter(None, paths))):
            path = ROOT / relative
            if not path.is_file() or "__pycache__" in path.parts or "target" in path.parts:
                continue
            if path.is_symlink():
                raise ValueError(f"release source must be a regular file: {relative}")
            data = path.read_bytes()
            hashes[relative] = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
            entry = tarfile.TarInfo(f"{name}/{relative}")
            entry.size = len(data)
            entry.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
            archive.addfile(entry, io.BytesIO(data))
    compressed = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=compressed, mtime=0) as stream:
        stream.write(raw.getvalue())
    packed = compressed.getvalue()
    receipt = {"package": "aq-session-host", "version": version, "host_plan_version": 1,
               "owner": "aq-worker", "archive": name + ".tar.gz",
               "sha256": hashlib.sha256(packed).hexdigest(), "bytes": len(packed), "files": hashes}
    return name, packed, (json.dumps(receipt, indent=2, sort_keys=True) + "\n").encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    name, packed, receipt = publication()
    for filename, data in [(name + ".tar.gz", packed), (name + ".json", receipt)]:
        path = RELEASE / filename
        if args.check:
            if path.read_bytes() != data:
                raise SystemExit(f"publication drift: {path}")
        else:
            RELEASE.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    print(json.dumps({k: v for k, v in json.loads(receipt).items() if k != "files"}, sort_keys=True))


if __name__ == "__main__":
    main()
