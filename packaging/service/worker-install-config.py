#!/usr/bin/env python3
"""Resolve the bundled template and finish the newly enrolled worker's configuration."""
import hashlib
import os
import re
import subprocess
import urllib.request
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


def manual_settings(package, podman, slots, source_slugs):
    lines = [
        "", "[manual]", f"podman_executable = {json.dumps(str(podman))}",
        f"max_slots = {slots}", "", "[manual.source_slugs]",
    ]
    lines.extend(f"{json.dumps(key)} = {json.dumps(slug)}" for key, slug in source_slugs.items())
    if (package / "codex-bundle").is_dir():
        lines.extend([
            "", "[manual.codex]", 'executable = "/opt/aq/bin/codex"',
            f'auth_file = {json.dumps(str(Path.home() / ".codex/auth.json"))}',
            f'runtime_bundle = {json.dumps(str(package / "codex-bundle"))}',
        ])
    return "\n" + "\n".join(lines) + "\n"


def configure(package, config, origin, podman, slots, digest):
    service = config / "service.toml"
    contents = service.read_text()
    current = tomllib.loads(contents)
    if current["slots"] != slots:
        raise ValueError("The profile already has a different slot count. Keep the existing count or choose another profile.")
    metadata = json.loads((package / "bootstrap.json").read_text())
    measurement = current.get("measurement", {})
    if metadata["manual_sessions"]:
        suffix = manual_settings(package, podman, slots, metadata["source_slugs"])
        if measurement.get("token_file"):
            if "manual" not in current:
                atomic_text(service, contents.rstrip() + suffix)
        elif "manual" in current:
            # Earlier installers appended this section before checking for the required token.
            if not contents.endswith(suffix):
                raise ValueError("This profile has custom manual settings. Configure its measurement token to enable manual sessions.")
            atomic_text(service, contents.removesuffix(suffix) + "\n")
    atomic_text(config / "hosted-install.json", json.dumps({"origin": origin, "bundle_sha256": digest}) + "\n")
    if not measurement.get("enabled"):
        print("The worker can connect now. Use Push logins on the worker in Agents → Swarm to enable agents.")



def set_toml_value(contents, section, key, value):
    """Change one scalar while retaining enrollment and unrelated settings verbatim."""
    pattern = r"(?m)^\[" + re.escape(section) + r"\]\s*$"
    found = re.search(pattern, contents)
    if not found:
        return contents.rstrip() + "\n\n[" + section + "]\n" + key + " = " + json.dumps(value) + "\n"
    tail = re.search(r"(?m)^\[", contents[found.end():])
    end = found.end() + tail.start() if tail else len(contents)
    block = contents[found.end():end]
    line = key + " = " + json.dumps(value)
    existing = r"(?m)^" + re.escape(key) + r"\s*=.*$"
    block = re.sub(existing, lambda _: line, block) if re.search(existing, block) else block.rstrip() + "\n" + line + "\n"
    return contents[:found.end()] + block + contents[end:]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args):
        raise ValueError("The worker endpoint must not redirect login delivery.")


def login_request(config, body, slot=1):
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]
    if not re.fullmatch(r"https://[a-zA-Z0-9][a-zA-Z0-9.-]*(?::[0-9]{1,5})?", origin):
        raise ValueError("Use the trusted HTTPS worker endpoint.")
    enrollment = read_toml(config / f"slots/{slot}/worker.toml")
    if enrollment["server_url"] != origin:
        raise ValueError("The worker endpoint changed. Reinstall from Add machine.")
    headers = {"Authorization": "Bearer " + enrollment["credential"], "Content-Type": "application/json"}
    if enrollment.get("cf_client_id") and enrollment.get("cf_client_secret"):
        headers.update({"CF-Access-Client-Id": enrollment["cf_client_id"], "CF-Access-Client-Secret": enrollment["cf_client_secret"]})
    request = urllib.request.Request(origin + "/v1/worker/logins", data=json.dumps(body).encode(), headers=headers)
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=12) as response:
        payload = response.read(262145)
        if len(payload) > 262144:
            raise ValueError("Login delivery exceeded its size limit.")
        return json.loads(payload)


def apply_logins(package, config, logins):
    providers = {}
    if not isinstance(logins, list) or not 1 <= len(logins) <= 2:
        raise ValueError("Invalid login delivery.")
    for login in logins:
        provider, material = login["provider"], login["material"]
        if provider not in ("anthropic", "openai") or provider in providers or not isinstance(material, str) or not 0 < len(material.encode()) <= 65536:
            raise ValueError("Invalid login delivery.")
        if provider == "anthropic" and not all(33 <= ord(c) <= 126 for c in material):
            raise ValueError("Invalid Claude subscription login.")
        if provider == "openai":
            auth = json.loads(material)
            if not isinstance(auth, dict) or set(auth) - {"auth_mode", "OPENAI_API_KEY", "tokens", "last_refresh"} or auth.get("auth_mode") != "chatgpt" or auth.get("OPENAI_API_KEY") is not None:
                raise ValueError("Invalid Codex subscription login.")
            tokens = auth.get("tokens", {})
            if not isinstance(tokens, dict) or set(tokens) != {"access_token", "refresh_token", "id_token", "account_id"}:
                raise ValueError("Invalid Codex subscription login.")
            for key, value in tokens.items():
                bound = 512 if key == "account_id" else 32768
                if not isinstance(value, str) or not 0 < len(value) <= bound or not all(33 <= ord(c) <= 126 for c in value):
                    raise ValueError("Invalid Codex subscription login.")
            refresh = auth.get("last_refresh")
            if refresh is not None and (not isinstance(refresh, str) or len(refresh) > 128 or not all(33 <= ord(c) <= 126 for c in refresh)):
                raise ValueError("Invalid Codex subscription login.")
            if not (package / "codex-bundle").is_dir():
                raise ValueError("This worker bundle does not include Codex.")
        providers[provider] = material
    private = config / "provider-auth"
    if private.is_symlink():
        raise ValueError("The private login directory must not be a link.")
    private.mkdir(mode=0o700, exist_ok=True)
    private.chmod(0o700)
    filenames = {"anthropic": "claude-token", "openai": "codex-auth.json"}
    new_provider = any(not (private / filenames[provider]).exists() for provider in providers)
    for provider, material in providers.items():
        atomic_text(private / filenames[provider], material)
    service = config / "service.toml"
    previous = service.read_text()
    contents = previous
    current = tomllib.loads(contents)
    claude = str(private / "claude-token")
    if "anthropic" in providers or not current.get("measurement", {}).get("token_file"):
        contents = set_toml_value(contents, "measurement", "token_file", claude)
    metadata = json.loads((package / "bootstrap.json").read_text())
    if "manual" not in current:
        settings = json.loads((config / "login-sync.json").read_text())
        contents = contents.rstrip() + manual_settings(package, settings["podman"], current["slots"], metadata["source_slugs"])
    if "openai" in providers:
        contents = set_toml_value(contents, "manual.codex", "auth_file", str(private / "codex-auth.json"))
    if contents != previous:
        atomic_text(service, contents)
    return contents != previous or new_provider


def sync_logins(package, config):
    state = read_toml(config / "service.toml")
    failed = False
    for slot in range(1, state["slots"] + 1):
        try:
            sync_slot(package, config, slot)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError):
            # Retiring a primary node must not prevent sibling nodes receiving logins.
            failed = True
    if failed:
        raise ValueError("Login delivery is waiting or needs attention. Check Push logins in the dashboard.")


def sync_slot(package, config, slot):
    response = login_request(config, {}, slot)
    generation = response.get("generation")
    if not generation:
        return
    import uuid
    generation = str(uuid.UUID(generation))
    receipt = config / f"login-receipt-{slot}.json"
    prior = json.loads(receipt.read_text()) if receipt.exists() else {}
    if prior.get("generation") != generation or not prior.get("applied"):
        try:
            changed = apply_logins(package, config, response["logins"])
            # Files at the same configured path are read for every new agent; only new
            # provider capabilities/configuration require a service restart.
            state = read_toml(config / "service.toml")
            if changed or prior.get("restart_pending"):
                atomic_text(receipt, json.dumps({"generation": generation, "restart_pending": True}))
                subprocess.run(["systemctl", "--user", "restart", "aq-node@" + state["profile"] + ".service"], check=True, timeout=65, capture_output=True)
            atomic_text(receipt, json.dumps({"generation": generation, "applied": True}))
        except Exception:
            login_request(config, {"generation": generation, "applied": False}, slot)
            raise ValueError("Login installation failed. Check the worker configuration and retry Push logins.") from None
    login_request(config, {"generation": generation, "applied": True}, slot)


def install_sync(package, config):
    state = read_toml(config / "service.toml")
    profile = state["profile"]
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,31}", profile):
        raise ValueError("Invalid worker profile.")
    settings = {"podman": str(Path.home() / ".local/opt/aq-sandbox-podman/podman-linux-amd64/usr/local/bin/podman")}
    atomic_text(config / "login-sync.json", json.dumps(settings))
    helper = config / "login-sync.py"
    if Path(__file__).resolve() != helper.resolve():
        atomic_text(helper, Path(__file__).read_text())
    unit_root = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    unit_root.mkdir(parents=True, exist_ok=True)
    # systemd specifiers and quoting are escaped independently of shell syntax.
    def unit_arg(value):
        return '"' + str(value).replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"') + '"'
    command = " ".join(unit_arg(v) for v in (sys.executable, helper, "sync", package, config))
    name = "aq-login-sync-" + profile
    atomic_text(unit_root / (name + ".service"), "[Unit]\nDescription=AQ worker login delivery\n[Service]\nType=oneshot\nUMask=0077\nExecStart=" + command + "\nTimeoutStartSec=240\n")
    atomic_text(unit_root / (name + ".timer"), "[Unit]\nDescription=Receive AQ logins for " + profile + "\n[Timer]\nOnBootSec=5\nOnUnitInactiveSec=10\n[Install]\nWantedBy=timers.target\n")
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True, timeout=15, capture_output=True)
    subprocess.run(["systemctl", "--user", "enable", "--now", name + ".timer"], check=True, timeout=15, capture_output=True)


def main():
    mode, package, *args = sys.argv[1:]
    package = Path(package)
    if mode == "prepare":
        work, origin, socket, config, digest = args
        prepare(package, Path(work), origin, Path(socket), Path(config), digest)
    elif mode == "configure":
        config, origin, podman, slots, digest = args
        configure(package, Path(config), origin, Path(podman), int(slots), digest)
    elif mode == "install-sync":
        install_sync(package, Path(args[0]))
    elif mode == "sync":
        sync_logins(package, Path(args[0]))
    else:
        raise ValueError("Unknown worker install configuration step.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        # TOML parse errors may contain fragments of a credential-bearing file.
        print("Worker configuration failed. " + (str(error) if type(error) is ValueError else "Check the local AQ profile and bundle."), file=sys.stderr)
        sys.exit(2)
