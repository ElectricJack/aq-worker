#!/usr/bin/env python3
"""Resolve the bundled template and finish the newly enrolled worker's configuration."""
import hashlib
import shutil
import tarfile
import http.client
import socket
import uuid
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

NETWORK_TOOLS_SHA256 = "bc4e3950d651996e0aa9579577b96b3d76e1b51431a4fe109a3ebba21d60720b"


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




def sandbox_preflight(package, config, podman):
    """Exercise the sandbox without a provider login or model invocation."""
    native = json.loads((config / "native.json").read_text())
    docker = native["executor"]["settings"]["host"]["docker_local"]
    class Connection(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.settimeout(15)
            self.sock.connect(docker["socket"])
    connection = Connection("localhost", timeout=15)
    try:
        connection.request("GET", "/v1.44/info")
        response = connection.getresponse()
        raw = response.read(1048577)
        if response.status != 200 or len(raw) > 1048576:
            raise ValueError("Sandbox daemon did not provide its resource configuration.")
        info = json.loads(raw)
    finally:
        connection.close()
    if not any("name=rootless" in option.split(",") for option in info.get("SecurityOptions", [])):
        raise ValueError("AQ's sandbox daemon must run as your normal Linux user.")
    if info.get("CgroupVersion") != "2" or info.get("CgroupDriver") in (None, "", "none"):
        raise ValueError("The sandbox needs cgroup v2 with a resource controller. Check that WSL uses systemd and restart WSL before retrying.")
    image, = docker["templates"]
    probe = "aq-install-check-" + uuid.uuid4().hex[:12]
    cli = [str(podman), "--remote", "--url", "unix://" + docker["socket"]]
    # Match the agent's runtime, bridge network and resource controls. No provider
    # credential is mounted and no agent executable is invoked.
    script = "set -eu; git --version; tmux -S /tmp/aq-install.sock new-session -d -s check 'sleep 10'; tmux -S /tmp/aq-install.sock has-session -t check; tmux -S /tmp/aq-install.sock kill-server"
    args = cli + ["run", "--rm", "--name", probe, "--label", "aq.install-check=true", "--network", "bridge", "--memory", "4g", "--memory-swap", "4g", "--cpus", "2", "--pids-limit", "512", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--ipc", "private", "--entrypoint", "/bin/bash", image["image"], "-c", script]
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=90)
        if result.returncode:
            # This fixed probe carries no secrets. Preserve the engine's useful
            # diagnostic instead of claiming installation succeeded.
            detail = result.stderr.strip()[-4096:] or "The sandbox test failed without an engine diagnostic."
            raise ValueError("Sandbox startup check failed:\n" + detail)
    except subprocess.TimeoutExpired:
        raise ValueError("Sandbox startup check did not finish. Check the AQ sandbox service and rerun the installer.") from None
    finally:
        try:
            subprocess.run(cli + ["rm", "--force", probe], capture_output=True, timeout=20)
        except (OSError, subprocess.SubprocessError):
            pass  # Do not replace the startup diagnostic with a cleanup error.
    print("Sandbox startup, bridge networking, resource controls and tmux passed.")

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


def worker_request(config, body, slot=1, endpoint="/v1/worker/logins"):
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
    request = urllib.request.Request(origin + endpoint, data=json.dumps(body).encode(), headers=headers)
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=12) as response:
        payload = response.read(262145)
        if len(payload) > 262144:
            raise ValueError("Login delivery exceeded its size limit.")
        return json.loads(payload)



def login_request(config, body, slot=1):
    return worker_request(config, body, slot)


def diagnostic_request(config, body, slot=1):
    return worker_request(config, body, slot, "/v1/worker/diagnostics")


def receiver_update(config, digest):
    if not isinstance(digest, str) or not re.fullmatch(r"[a-f0-9]{64}", digest):
        raise ValueError("Receiver update needs a pinned installer checksum.")
    target = config / "login-sync.py"
    if Path(__file__).resolve() != target.resolve():
        raise ValueError("Receiver updates must run from the installed receiver.")
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]  # worker_request has already verified the enrollment's HTTPS origin.
    with urllib.request.build_opener(NoRedirect()).open(origin + "/install/" + digest + ".sh", timeout=20) as response:
        content = response.read(262145)
    if len(content) > 262144 or hashlib.sha256(content).hexdigest() != digest:
        raise ValueError("Receiver update checksum did not match.")
    text = content.decode("utf-8")
    prefix, suffix = "<<'AQ_INSTALL_CONFIG'\n", "\nAQ_INSTALL_CONFIG\n"
    if text.count(prefix) != 1 or suffix not in text:
        raise ValueError("The pinned installer does not contain a receiver.")
    helper = text.split(prefix, 1)[1].split(suffix, 1)[0]
    compile(helper, str(target), "exec")
    atomic_text(target, helper + "\n")
    return {"ok": True, "phase": "receiver_update", "details": {"installer_sha256": digest}}


def sandbox_probe(config):
    """Probe only AQ's configured sandbox; never read provider or node credentials."""
    service = read_toml(config / "service.toml")
    native = json.loads(Path(service["native_template"]).read_text())
    docker = native["executor"]["settings"]["host"]["docker_local"]
    socket_path = Path(docker["socket"])
    if not socket_path.is_absolute() or socket_path.resolve() in (Path("/var/run/docker.sock"), Path("/run/docker.sock"), Path("/run/podman/podman.sock")) or socket_path.stat().st_uid != os.getuid():
        raise ValueError("The AQ sandbox socket must belong to the worker user.")
    details = {"socket": str(socket_path), "shared_root_fs": docker.get("shared_root_fs", False)}
    recent = []
    state_root = Path(service["state_root"]) / "manual-agents"
    if state_root.is_dir():
        paths = [p / "state.json" for p in state_root.iterdir() if p.is_dir() and not p.is_symlink() and (p / "state.json").is_file() and not (p / "state.json").is_symlink()]
        for path in sorted(paths, key=lambda p: p.stat().st_mtime, reverse=True)[:4]:
            if path.stat().st_size > 65536:
                continue
            try:
                value = json.loads(path.read_text())
                identifier = str(uuid.UUID(value["id"]))
                recent.append({"id": identifier, "status": value.get("status") if value.get("status") in ("pending", "preparing", "running", "stopping", "stopped", "failed") else "unknown", "sandbox_created": isinstance(value.get("sandbox"), dict), "provider": value.get("spec", {}).get("provider") if value.get("spec", {}).get("provider") in ("anthropic", "openai") else "unknown"})
            except (ValueError, OSError, KeyError, TypeError):
                continue
    details["recent_sessions"] = recent
    phase = "daemon_info"
    class Connection(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.settimeout(20)
            self.sock.connect(str(socket_path))
    def call(method, path, body=None):
        connection = Connection("localhost", timeout=20)
        try:
            connection.request(method, "/v1.44" + path, body=None if body is None else json.dumps(body).encode(), headers={} if body is None else {"Content-Type": "application/json"})
            response = connection.getresponse()
            raw = response.read(65537)
            if len(raw) > 65536:
                raise ValueError("The sandbox diagnostic response exceeded its limit.")
            return response.status, raw
        finally:
            connection.close()
    def expected(method, path, body, statuses):
        status, raw = call(method, path, body)
        if status not in statuses:
            try:
                message = json.loads(raw).get("message", "Sandbox API refused the fixed test")
            except (ValueError, AttributeError):
                message = "Sandbox API refused the fixed test"
            raise ValueError("Sandbox API " + str(status) + ": " + str(message)[:4096])
        return raw
    probe = "aq-worker-check-" + uuid.uuid4().hex[:12]
    volume = probe + "-work"
    made_volume = made_container = False
    try:
        raw = expected("GET", "/info", None, (200,))
        info = json.loads(raw)
        for key in ("CgroupDriver", "CgroupVersion", "SecurityOptions", "DockerRootDir"):
            details[key] = info.get(key)
        if not any("name=rootless" in option.split(",") for option in info.get("SecurityOptions", [])):
            raise ValueError("The sandbox daemon did not report rootless mode.")
        if info.get("CgroupDriver") in (None, "", "none") or info.get("CgroupVersion") != "2":
            raise ValueError("The worker requires a cgroup v2 resource controller.")
        storage = info.get("DockerRootDir")
        shared = not storage or not Path(storage).exists() or Path(storage).stat().st_dev == Path("/").stat().st_dev
        details["storage_shares_root"] = shared
        if shared and not docker.get("shared_root_fs", False):
            raise ValueError("Sandbox storage shares the root filesystem but this installation has no shared_root_fs override.")
        image, = docker["templates"]
        details["image"] = image["image"]
        phase = "image"
        from urllib.parse import quote
        expected("GET", "/images/" + quote(image["image"], safe="") + "/json", None, (200,))
        phase = "volume_create"
        expected("POST", "/volumes/create", {"Name": volume, "Driver": "local", "Labels": {"aq.install-check": "true"}}, (200, 201))
        made_volume = True
        phase = "container_create"
        script = "set -eu; git --version; tmux -S /tmp/aq-check.sock new-session -d -s check 'sleep 10'; tmux -S /tmp/aq-check.sock has-session -t check; tmux -S /tmp/aq-check.sock kill-server"
        body = {"Image": image["image"], "Labels": {"aq.install-check": "true"}, "Env": [], "WorkingDir": "/aq/work", "ExposedPorts": {}, "AttachStdin": False, "OpenStdin": False, "Tty": False, "Cmd": ["/bin/bash", "-c", script], "HostConfig": {"Privileged": False, "CapAdd": [], "CapDrop": ["ALL"], "Devices": [], "DeviceCgroupRules": [], "SecurityOpt": ["no-new-privileges"], "NanoCpus": 2000000000, "Memory": 4294967296, "MemorySwap": 4294967296, "PidsLimit": 512, "NetworkMode": "bridge", "PortBindings": {}, "PublishAllPorts": False, "Binds": [], "Mounts": [{"Type": "volume", "Source": volume, "Target": "/aq/work", "ReadOnly": False}], "IpcMode": "private", "RestartPolicy": {"Name": "no"}, "AutoRemove": False}}
        expected("POST", "/containers/create?name=" + probe, body, (201,))
        made_container = True
        phase = "container_start"
        expected("POST", "/containers/" + probe + "/start", None, (204, 304))
        phase = "tmux"
        import time
        for _ in range(30):
            container = json.loads(expected("GET", "/containers/" + probe + "/json", None, (200,)))
            state = container.get("State", {})
            if state.get("Status") in ("exited", "dead"):
                if state.get("ExitCode") != 0:
                    _status, raw = call("GET", "/containers/" + probe + "/logs?stdout=1&stderr=1")
                    # Docker log frames contain fixed-probe output only, never an agent session.
                    logs = bytearray()
                    while len(raw) >= 8 and raw[0] in (0, 1, 2) and raw[1:4] == b"\0\0\0":
                        size = int.from_bytes(raw[4:8], "big"); logs.extend(raw[8:8+size]); raw = raw[8+size:]
                    logs.extend(raw)
                    raise ValueError("The fixed sandbox check exited with code " + str(state.get("ExitCode")) + ": " + logs.decode("utf-8", errors="replace")[-4096:])
                return {"ok": True, "phase": "complete", "details": details}
            time.sleep(1)
        raise ValueError("The fixed sandbox check has not exited.")
    except (ValueError, OSError, KeyError, TypeError, http.client.HTTPException) as error:
        # No credentials are read or mounted by this probe. These errors describe
        # only its own daemon request, filesystem metadata, or fixed command.
        return {"ok": False, "phase": phase, "error": str(error)[:4096], "details": details}
    finally:
        try:
            if made_container:
                call("DELETE", "/containers/" + probe + "?force=true")
            if made_volume:
                call("DELETE", "/volumes/" + volume)
        except (OSError, ValueError, http.client.HTTPException):
            pass



def install_network_tools(config):
    """Restore an AQ-owned runtime dependency without sudo or system packages."""
    state = read_toml(config / "service.toml")
    prefix = Path.home() / ".local/opt/aq-sandbox-podman"
    expected = prefix / "podman-linux-amd64/usr/local/bin/podman"
    configured = Path(state.get("manual", {}).get("podman_executable", "/unconfigured"))
    if configured != expected or not prefix.is_dir() or prefix.is_symlink() or prefix.stat().st_uid != os.getuid():
        raise ValueError("Network helper repair is restricted to the installed AQ sandbox runtime.")
    kill_mode = subprocess.run(["systemctl", "--user", "show", "aq-sandbox-podman.service", "--property=KillMode", "--value"], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
    if kill_mode != "process":
        raise ValueError("The sandbox service must preserve running containers before it can be restarted.")
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]
    if not re.fullmatch(r"https://[a-zA-Z0-9][a-zA-Z0-9.-]*(?::[0-9]{1,5})?", origin):
        raise ValueError("Network tools require the installed HTTPS worker origin.")
    root = prefix / "network-tools"
    root.mkdir(mode=0o700, exist_ok=True)
    target = root / NETWORK_TOOLS_SHA256
    if not target.exists():
        with urllib.request.build_opener(NoRedirect()).open(origin + "/install/" + NETWORK_TOOLS_SHA256 + ".tar.gz", timeout=30) as response:
            contents = response.read(8388609)
        if len(contents) > 8388608 or hashlib.sha256(contents).hexdigest() != NETWORK_TOOLS_SHA256:
            raise ValueError("AQ network tools failed checksum verification.")
        with tempfile.TemporaryDirectory(prefix="stage-", dir=root) as stage:
            stage = Path(stage)
            archive = stage / "tools.tar.gz"; archive.write_bytes(contents)
            unpacked = stage / "runtime"; unpacked.mkdir()
            with tarfile.open(archive) as tar:
                members = tar.getmembers()
                if len(members) > 512 or sum(m.size for m in members) > 33554432 or any(not (m.isfile() or m.isdir()) for m in members):
                    raise ValueError("AQ network tools archive was not a supported runtime.")
                tar.extractall(unpacked, filter="data")
            if not (unpacked / "bin/iptables").is_file() or not (unpacked / "manifest.json").is_file():
                raise ValueError("AQ network tools archive is incomplete.")
            unpacked.rename(target)
    manifest = json.loads((target / "manifest.json").read_text())
    for name, fact in manifest["files"].items():
        path = (target / name).resolve()
        if not path.is_relative_to(target.resolve()) or not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != fact["sha256"]:
            raise ValueError("Installed AQ network tools do not match their manifest.")
    unitroot = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user/aq-sandbox-podman.service.d"
    unitroot.mkdir(parents=True, exist_ok=True)
    path = str(target / "bin") + ":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    path = path.replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"')
    atomic_text(unitroot / "30-aq-network-tools.conf", '[Service]\nEnvironment="PATH=' + path + '"\n')
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True, capture_output=True, timeout=15)
    subprocess.run(["systemctl", "--user", "restart", "aq-sandbox-podman.service"], check=True, capture_output=True, timeout=30)
    return NETWORK_TOOLS_SHA256


def sandbox_diagnostics(config):
    report = sandbox_probe(config)
    missing = not report.get("ok") and report.get("phase") == "container_start" and "iptables" in report.get("error", "") and "No such file or directory" in report.get("error", "")
    if not missing:
        return report
    try:
        restored = install_network_tools(config)
        import time
        for attempt in range(20):
            try:
                retry = sandbox_probe(config)
                if retry.get("phase") != "daemon_info":
                    break
            except OSError:
                if attempt == 19:
                    raise
            time.sleep(0.5)
        retry.setdefault("details", {})["network_tools_restored"] = restored
        return retry
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        message = str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "AQ's private network helper repair failed."
        return {"ok": False, "phase": "network_support", "error": message, "details": report.get("details", {})}

def sync_diagnostics_slot(package, config, slot):
    job = diagnostic_request(config, {}, slot)
    operation = job.get("operation_id")
    if not operation:
        return
    operation = str(uuid.UUID(operation))
    receipt = config / f"diagnostic-receipt-{slot}.json"
    prior = json.loads(receipt.read_text()) if receipt.exists() else {}
    if prior.get("operation_id") == operation:
        report = prior["report"]
    else:
        try:
            if job.get("kind") == "sandbox_startup_v1":
                report = sandbox_diagnostics(config)
            elif job.get("kind") == "receiver_update_v1":
                report = receiver_update(config, job.get("installer_sha256"))
            else:
                raise ValueError("This receiver does not support the requested check.")
        except (ValueError, OSError, KeyError, TypeError) as error:
            report = {"ok": False, "phase": "receiver", "error": str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "The receiver could not complete its fixed check."}
        atomic_text(receipt, json.dumps({"operation_id": operation, "report": report}))
    diagnostic_request(config, {"operation_id": operation, "report": report}, slot)


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
    try:
        state = read_toml(config / "service.toml")
        count = state["slots"]
        if type(count) is not int or not 1 <= count <= 8:
            raise ValueError("Invalid slot count")
        slots = list(range(1, count + 1))
    except (ValueError, OSError, KeyError):
        # Control traffic must survive the broken configuration it diagnoses.
        slots = [slot for slot in range(1, 9) if (config / f"slots/{slot}/worker.toml").is_file()]
    failed = False
    for slot in slots:
        for receive in (sync_slot, sync_diagnostics_slot):
            try:
                receive(package, config, slot)
            except (OSError, ValueError, KeyError, subprocess.SubprocessError, http.client.HTTPException):
                # One failing function/node must not prevent other requests.
                failed = True
    if failed:
        raise ValueError("Worker controls are waiting or need attention. Check this worker in the dashboard.")


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



def upgrade_controls(config, origin):
    """Upgrade an already connected receiver without reinstalling its runtime."""
    profile = config.name
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,31}", profile):
        raise ValueError("Invalid worker profile.")
    try:
        marker = json.loads((config / "hosted-install.json").read_text())
        slot = read_toml(config / "slots/1/worker.toml")
        if marker["origin"] != origin or slot["server_url"] != origin:
            raise ValueError("This worker belongs to another server.")
        target = config / "login-sync.py"
        if not target.is_file() or target.is_symlink() or target.stat().st_uid != os.getuid():
            raise ValueError("The installed AQ control receiver was not found for this profile.")
        atomic_text(target, Path(__file__).read_text())
        subprocess.run(["systemctl", "--user", "restart", "aq-login-sync-" + profile + ".timer"], check=True, timeout=20, capture_output=True)
        subprocess.run(["systemctl", "--user", "start", "--no-block", "aq-login-sync-" + profile + ".service"], check=True, timeout=20, capture_output=True)
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        # Bootstrap may fail before its timer is running. Return its own fixed
        # setup diagnostic over the existing enrollment whenever possible.
        message = str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "The AQ control service could not be started."
        try:
            job = diagnostic_request(config, {})
            if job.get("operation_id"):
                diagnostic_request(config, {"operation_id": job["operation_id"], "report": {"ok": False, "phase": "controls_upgrade", "error": message}})
        except (ValueError, OSError, KeyError):
            pass
        raise ValueError(message) from None
    print("Worker controls updated. AQ will collect the queued diagnostic directly.")

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
    elif mode == "preflight":
        sandbox_preflight(package, Path(args[0]), Path(args[1]))
    elif mode == "upgrade-controls":
        upgrade_controls(Path(args[0]), args[1])
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
