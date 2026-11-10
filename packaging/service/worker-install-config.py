#!/usr/bin/env python3
"""Resolve the bundled template and finish the newly enrolled worker's configuration."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import tomllib
from urllib.parse import urlsplit


def atomic_text(path, contents):
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as stream:
        staging = Path(stream.name)
        stream.write(contents)
    try:
        staging.replace(path)
    finally:
        staging.unlink(missing_ok=True)


def read_toml(path):
    with path.open("rb") as stream:
        return tomllib.load(stream)


def prepare(package, work, origin, socket, config, digest):
    marker = config / "hosted-install.json"
    if marker.exists() and json.loads(marker.read_text()) != {"origin": origin, "bundle_sha256": digest}:
        raise ValueError("This profile was installed from another bundle or server. Use a different --profile.")
    template = json.loads((package / "native.json").read_text())
    host = template["executor"]["settings"]["host"]
    host["root"] = str(config / "host")
    host["docker_local"]["socket"] = str(socket)
    entry, = host["docker_local"]["templates"]
    repo = entry["image"].split("/", 1)[1]
    entry["image"] = urlsplit(origin).netloc + "/" + repo
    for key, name in [("driver", "aq-sandbox-driver"), ("helper", "aq-session-host"), ("tmux", "tmux")]:
        host[key]["path"] = str(package / name)
    template["adapter_manifest"] = str(package / "adapter.json")
    template["executor"]["composite_definition"] = str(package / "composite.json")
    template["inventory_definition"] = str(package / "inventory.json")
    template.update(state_dir=str(config / "state"), git_root=str(config / "git"), git_socket=str(config / "git.sock"))
    native = json.dumps(template, indent=2) + "\n"
    enrolled = config / "service.toml"
    if enrolled.exists():
        slot = read_toml(config / "slots/1/worker.toml")
        if slot["server_url"] != origin or (config / "native.json").read_text() != native:
            raise ValueError("This profile is already configured for a different installation. Use a different --profile.")
    # aq-install.sh shares these paths across its profiles. Refuse a binary swap
    # on a machine that already uses a different runtime.
    runtime = Path.home() / ".local/share/agent-q/runtime"
    for name in ("aq-worker", "aq-session-host", "aq-agent", "aq-git", "aq-sandbox-driver"):
        installed = runtime / name
        if installed.exists():
            with installed.open("rb") as current, (package / name).open("rb") as source:
                if hashlib.file_digest(current, "sha256").digest() != hashlib.file_digest(source, "sha256").digest():
                    raise ValueError("Another AQ runtime is installed. Install on a fresh WSL distro or upgrade that runtime separately.")
    (work / "native.json").write_text(native)
    (work / "image").write_text(entry["image"] + "\n")


def configure(package, config, origin, podman, slots, digest):
    service = config / "service.toml"
    current = read_toml(service)
    if current["slots"] != slots:
        raise ValueError("The profile already has a different slot count. Keep the existing count or choose another profile.")
    metadata = json.loads((package / "bootstrap.json").read_text())
    if metadata["manual_sessions"] and "manual" not in current:
        lines = [
            "", "[manual]", f"podman_executable = {json.dumps(str(podman))}",
            f"max_slots = {slots}", "", "[manual.source_slugs]",
        ]
        lines.extend(f"{json.dumps(key)} = {json.dumps(slug)}" for key, slug in metadata["source_slugs"].items())
        if (package / "codex-bundle").is_dir():
            lines.extend([
                "", "[manual.codex]", 'executable = "/opt/aq/bin/codex"',
                f'auth_file = {json.dumps(str(Path.home() / ".codex/auth.json"))}',
                f'runtime_bundle = {json.dumps(str(package / "codex-bundle"))}',
            ])
        atomic_text(service, service.read_text().rstrip() + "\n" + "\n".join(lines) + "\n")
    atomic_text(config / "hosted-install.json", json.dumps({"origin": origin, "bundle_sha256": digest}) + "\n")
    if not current.get("measurement", {}).get("enabled"):
        print("The node can enroll without a provider login. Set up its subscription before running agents.")


def main():
    mode, package, *args = sys.argv[1:]
    package = Path(package)
    if mode == "prepare":
        work, origin, socket, config, digest = args
        prepare(package, Path(work), origin, Path(socket), Path(config), digest)
    elif mode == "configure":
        config, origin, podman, slots, digest = args
        configure(package, Path(config), origin, Path(podman), int(slots), digest)
    else:
        raise ValueError("Unknown worker install configuration step.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError) as error:
        # TOML parse errors may contain fragments of a credential-bearing file.
        print("Worker configuration failed. " + (str(error) if type(error) is ValueError else "Check the local AQ profile and bundle."), file=sys.stderr)
        sys.exit(2)
