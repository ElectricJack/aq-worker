#!/usr/bin/env python3
"""Verify exact independent010/011 payloads, keeping old frozen packages intact."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PINS = {
    "aq-admission-0.1.10": "bd10dda2220bc91a8f0999cbe2a6e12804bdd3137c145c779d690d504c46d78e",
    "aq-phase-terminal-protocol-0.1.1": "818f2499a2d50f47db491031f50fc1e629fd9d402e224a460efea1ea5532805c",
}
for package, archive_digest in PINS.items():
    metadata = json.loads((ROOT / "vendor" / (package + "-source.json")).read_text())
    assert metadata["archive_sha256"] == archive_digest
    folder = ROOT / "vendor" / package
    actual = {p.relative_to(folder).as_posix() for p in folder.rglob("*") if p.is_file()}
    expected = {entry["path"] for entry in metadata["files"]}
    assert actual == expected, (package, actual ^ expected)
    for entry in metadata["files"]:
        payload = (folder / entry["path"]).read_bytes()
        assert len(payload) == entry["bytes"]
        assert hashlib.sha256(payload).hexdigest() == entry["sha256"], entry["path"]
    vcs = json.loads((folder / ".cargo_vcs_info.json").read_text())
    assert vcs["git"]["sha1"] == metadata["source_revision"]
    assert not vcs["git"].get("dirty", False)
    print(f"{package}: {len(expected)} exact archive payloads; source {metadata['source_revision']}")
