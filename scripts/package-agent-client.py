#!/usr/bin/env python3
"""Build a local, host-target AQ client distribution; never publish or install it."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parent.parent

def sha(data):
    return hashlib.sha256(data).hexdigest()


def checked(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path, help="New .tar.gz path")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    pin = json.loads((ROOT / "vendor/agent-api-contract.json").read_bytes())
    package = ROOT / "vendor/aq-agent-api-0.1.0"
    if sha((ROOT / "vendor/aq-agent-api-0.1.0.crate").read_bytes()) != pin["archive_sha256"]:
        raise ValueError("public API archive digest changed")
    for path, expected in pin["files"].items():
        data = (package / path).read_bytes()
        if sha(data) != expected["sha256"] or len(data) != expected["bytes"]:
            raise ValueError("public API extracted package changed")
    command = ["cargo", "build", "--locked", "--release", "-p", "aq-agent-client", "--bin", "aq-agent", "--message-format=json"]
    if args.offline:
        command.append("--offline")
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, check=True)
    executable = None
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "aq-agent":
            executable = message.get("executable") or executable
    if not executable:
        raise ValueError("Cargo did not produce aq-agent")
    files = {"bin/aq-agent": Path(executable).read_bytes(),
             "README.md": (ROOT / "crates/aq-agent-client/README.md").read_bytes(),
             "agent-api-contract.json": (ROOT / "vendor/agent-api-contract.json").read_bytes()}
    for name in ["schema.json", "openapi.json", "cli-contract.json"]:
        files["contracts/" + name] = (package / "artifacts" / name).read_bytes()
    result_contract = ROOT / "crates/aq-agent-result-local"
    files["contracts/agent-result-local.json"] = (result_contract / "artifacts/contract.json").read_bytes()
    files["instructions/agent-result-startup.md"] = (result_contract / "artifacts/startup.md").read_bytes()
    manifest = {"format_version": 1, "component": "aq-agent-client", "version": "0.1.0",
                "source_revision": checked("git", "rev-parse", "HEAD"),
                "working_tree_dirty": bool(checked("git", "status", "--porcelain", "--untracked-files=all")),
                "rustc": checked("rustc", "-vV"), "agent_api_version": 1,
                "api_archive_sha256": pin["archive_sha256"],
                "runtime": "Linux host target; no portable libc or container qualification claim",
                "capabilities": ["context.live", "context.cached", "graph.read", "graph.command", "operation.get", "result.submit.local", "result.status.local"],
                "files": {name: {"sha256": sha(data), "bytes": len(data)} for name, data in sorted(files.items())}}
    files["manifest.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    # Exclusive creation prevents replacing an existing release. Deterministic
    # archive metadata makes byte-level replay possible for identical inputs.
    with args.output.open("xb") as output:
        with gzip.GzipFile(fileobj=output, filename="", mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for name, data in sorted(files.items()):
                    entry = tarfile.TarInfo(name)
                    entry.size = len(data)
                    entry.mode = 0o755 if name.startswith("bin/") else 0o644
                    archive.addfile(entry, io.BytesIO(data))
    print(json.dumps({"artifact": str(args.output), "sha256": sha(args.output.read_bytes()),
                      "source_revision": manifest["source_revision"],
                      "working_tree_dirty": manifest["working_tree_dirty"]}))


if __name__ == "__main__":
    main()
