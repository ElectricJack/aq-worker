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

HOST_BUNDLE_SHA256 = "@AQ_HOST_BUNDLE_SHA256@"

NETWORK_TOOLS_SHA256 = "bc4e3950d651996e0aa9579577b96b3d76e1b51431a4fe109a3ebba21d60720b"


def atomic_text(path, contents):
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as stream:
        staging = Path(stream.name)
        stream.write(contents)
    try:
        staging.replace(path)
    finally:
        staging.unlink(missing_ok=True)


def node_count(config):
    """The old service key `slots` counts enrolled nodes, never agents within a node."""
    return config.get("nodes", config.get("slots"))


def read_toml(path):
    with path.open("rb") as stream:
        return tomllib.load(stream)


def is_package_update(config, origin, digest):
    # A profile installed from this server's installer may take a newer bundle
    # from the same server: that is an update. Its runtime binaries and native
    # template are then replaced; its enrollments, node count and identities are kept.
    marker = config / "hosted-install.json"
    update = False
    if marker.exists():
        recorded = json.loads(marker.read_text())
        if recorded.get("origin") != origin:
            raise ValueError("This profile was installed from another server. Use a different --profile.")
        update = recorded.get("bundle_sha256") != digest
    return update


def prepared_template(package, config, origin, socket):
    template = json.loads((package / "native.json").read_text())
    host = template["executor"]["settings"]["host"]
    host["root"] = str(config / "host")
    host["docker_local"]["socket"] = str(socket)
    entry, = host["docker_local"]["templates"]
    repo = entry["image"].split("/", 1)[1]
    entry["image"] = urlsplit(origin).netloc + "/" + repo
    for key, name in [("helper", "aq-session-host"), ("tmux", "tmux")]:
        host[key]["path"] = str(package / name)
    template["adapter_manifest"] = str(package / "adapter.json")
    template["executor"]["composite_definition"] = str(package / "composite.json")
    template["inventory_definition"] = str(package / "inventory.json")
    # The worker validates these when it loads the template, before it replaces them with
    # each slot's own: the state directory must exist, be private to this user (0700) and
    # contain the git root. `configure` creates it.
    template.update(state_dir=str(config / "state"), git_root=str(config / "state/git"), git_socket=str(config / "git.sock"))
    # Only a node-git enrolled template has node_git; an unenrolled one keeps the bundle path.
    # One repository per project for the whole profile, shared by every slot.
    if "node_git" in template:
        template["node_git"]["repositories"] = str(config / "state/node-git")
    return template, entry["image"]


def prepare_definitions(template, package):
    definitions = ["adapter.json", "composite.json", "inventory.json"]
    if codex := template["executor"]["settings"].get("codex"):
        codex["composite_definition"] = str(package / "codex-composite.json")
        definitions.append("codex-composite.json")
        if codex.get("runtime_bundle") is not None:
            codex["runtime_bundle"] = str(package / "codex-bundle")
    for name in definitions:
        (package / name).chmod(0o600)  # the worker reads its definitions only as private files


def check_enrollment(config, origin, native, update):
    enrolled = config / "service.toml"
    if not enrolled.exists():
        return
    slot = read_toml(config / "slots/1/worker.toml")
    stored = (config / "native.json").read_text()
    # An install made before the state directory fix differs only in its git root.
    migrated = stored.replace(json.dumps(str(config / "git")), json.dumps(str(config / "state/git")))
    repaired = stored != native and migrated == native
    different_template = stored != native and not (update or repaired)
    if slot["server_url"] != origin or different_template:
        raise ValueError(
            "This profile is already configured for a different installation. Use a different --profile."
        )
    if update or repaired:
        # Taken up by `configure`, after the new binaries are in place.
        atomic_text(config / "native.json.update", native)


def check_shared_runtime(package, update):
    # aq-install.sh shares these paths across its profiles. Refuse a binary swap
    # on a machine that already uses a different runtime.
    runtime = Path.home() / ".local/share/agent-q/runtime"
    for name in ("aq-worker", "aq-session-host", "aq-agent", "aq-git"):
        installed = runtime / name
        if installed.exists():
            with installed.open("rb") as current, (package / name).open("rb") as source:
                if not update and hashlib.file_digest(current, "sha256").digest() != hashlib.file_digest(source, "sha256").digest():
                    raise ValueError("Another AQ runtime is installed. Install on a fresh WSL distro or upgrade that runtime separately.")


def prepare(package, work, origin, socket, config, digest):
    update = is_package_update(config, origin, digest)
    template, image = prepared_template(package, config, origin, socket)
    prepare_definitions(template, package)
    native = json.dumps(template, indent=2) + "\n"
    check_enrollment(config, origin, native, update)
    check_shared_runtime(package, update)
    (work / "native.json").write_text(native)
    (work / "image").write_text(image + "\n")


def manual_settings(package, podman, slots, source_slugs, claude_token):
    """The `[manual]` section. `claude_token_file` is the Claude subscription token file manual
    Claude sessions log in with; the worker requires the key and offers Claude only while the file
    is readable."""
    lines = [
        "", "[manual]", f"podman_executable = {json.dumps(str(podman))}",
        f"claude_token_file = {json.dumps(str(claude_token))}",
        "", "[manual.source_slugs]",
    ]
    lines.extend(f"{json.dumps(key)} = {json.dumps(slug)}" for key, slug in source_slugs.items())
    if (package / "codex-bundle").is_dir():
        lines.extend([
            # No login file: every Codex session leases AQ's saved login from the server.
            "", "[manual.codex]", 'executable = "/opt/aq/bin/codex"',
            f'runtime_bundle = {json.dumps(str(package / "codex-bundle"))}',
        ])
    return "\n" + "\n".join(lines) + "\n"


def configure(package, config, origin, podman, slots, digest, claude_token=None):
    """`claude_token` is a Claude subscription token file found on this machine at install time
    (none when it arrives later through Push Claude token)."""
    service = config / "service.toml"
    contents = service.read_text()
    current = tomllib.loads(contents)
    if node_count(current) != slots:
        raise ValueError("The profile already has a different node count. Keep the existing count or choose another profile.")
    metadata = json.loads((package / "bootstrap.json").read_text())
    manual = "manual" in current
    if metadata["manual_sessions"] and not manual and claude_token is not None:
        atomic_text(service, contents.rstrip() + manual_settings(package, podman, slots, metadata["source_slugs"], claude_token))
        manual = True
    # Without this directory the worker fails every attempt with "No such file or directory"
    # and never reports its capabilities.
    state = config / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    state.chmod(0o700)
    staged = config / "native.json.update"
    if staged.exists():
        staged.replace(config / "native.json")
        print("This worker was updated to the bundle of this installer. Its enrollment was kept.")
    atomic_text(config / "hosted-install.json", json.dumps({"origin": origin, "bundle_sha256": digest}) + "\n")
    if not manual:
        print("The worker can connect now. Use Push Claude token on the worker in Agents → Swarm to deliver its Claude login.")




def rootless_daemon(info):
    return any("name=rootless" in option.split(",") for option in info.get("SecurityOptions", []))


def cgroup_v2(info):
    return info.get("CgroupVersion") == "2" and info.get("CgroupDriver") not in (None, "", "none")


def sandbox_resource_info(docker):
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
    return info


def check_preflight_resources(info):
    if not rootless_daemon(info):
        raise ValueError("AQ's sandbox daemon must run as your normal Linux user.")
    if not cgroup_v2(info):
        raise ValueError("The sandbox needs cgroup v2 with a resource controller. Check that WSL uses systemd and restart WSL before retrying.")


def sandbox_preflight(package, config, podman):
    """Exercise the sandbox without a provider login or model invocation."""
    native = json.loads((config / "native.json").read_text())
    docker = native["executor"]["settings"]["host"]["docker_local"]
    check_preflight_resources(sandbox_resource_info(docker))
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


def enrollment_headers(enrollment):
    headers = {"Authorization": "Bearer " + enrollment["credential"], "Content-Type": "application/json"}
    if enrollment.get("cf_client_id") and enrollment.get("cf_client_secret"):
        headers.update({"CF-Access-Client-Id": enrollment["cf_client_id"], "CF-Access-Client-Secret": enrollment["cf_client_secret"]})
    return headers


def worker_request(config, body, slot=1, endpoint="/v1/worker/logins"):
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]
    if not re.fullmatch(r"https://[a-zA-Z0-9][a-zA-Z0-9.-]*(?::[0-9]{1,5})?", origin):
        raise ValueError("Use the trusted HTTPS worker endpoint.")
    enrollment = read_toml(config / f"slots/{slot}/worker.toml")
    if enrollment["server_url"] != origin:
        raise ValueError("The worker endpoint changed. Reinstall from Add or update machine.")
    headers = enrollment_headers(enrollment)
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


def fetch_installer(origin, digest):
    with urllib.request.build_opener(NoRedirect()).open(origin + "/install/" + digest + ".sh", timeout=20) as response:
        content = response.read(262145)
    if len(content) > 262144 or hashlib.sha256(content).hexdigest() != digest:
        raise ValueError("Receiver update checksum did not match.")
    return content.decode("utf-8")


def install_receiver(text, target):
    prefix, suffix = "<<'AQ_INSTALL_CONFIG'\n", "\nAQ_INSTALL_CONFIG\n"
    if text.count(prefix) != 1 or suffix not in text:
        raise ValueError("The pinned installer does not contain a receiver.")
    helper = text.split(prefix, 1)[1].split(suffix, 1)[0]
    compile(helper, str(target), "exec")
    atomic_text(target, helper + "\n")


def receiver_update(config, digest):
    if not isinstance(digest, str) or not re.fullmatch(r"[a-f0-9]{64}", digest):
        raise ValueError("Receiver update needs a pinned installer checksum.")
    target = config / "login-sync.py"
    if Path(__file__).resolve() != target.resolve():
        raise ValueError("Receiver updates must run from the installed receiver.")
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]  # worker_request has already verified the enrollment's HTTPS origin.
    text = fetch_installer(origin, digest)
    install_receiver(text, target)
    details = {"installer_sha256": digest}
    # The receiver is already replaced; a worker update that fails is reported, not raised.
    try:
        details["worker"] = worker_binary_update(config, text, origin)
    except (ValueError, OSError, KeyError, subprocess.SubprocessError, tarfile.TarError) as error:
        details["worker"] = {"error": str(error)[:1024]}
    return {"ok": True, "phase": "receiver_update", "details": details}


def download_worker_bundle(origin, digest, archive):
    limit, seen, checksum = 536870912, 0, hashlib.sha256()
    with urllib.request.build_opener(NoRedirect()).open(origin + "/install/" + digest + ".tar.gz", timeout=60) as response, open(archive, "wb") as out:
        while chunk := response.read(1 << 20):
            seen += len(chunk)
            if seen > limit:
                raise ValueError("The worker bundle is too large.")
            checksum.update(chunk)
            out.write(chunk)
    if checksum.hexdigest() != digest:
        raise ValueError("Worker bundle checksum did not match.")


def extract_worker_binary(archive, fresh):
    with tarfile.open(archive) as bundle:
        member = bundle.getmember("aq-worker")
        if not member.isfile():
            raise ValueError("The worker bundle holds no worker.")
        with bundle.extractfile(member) as source, open(fresh, "wb") as out:
            shutil.copyfileobj(source, out)


def release_refused_slots(config):
    # A slot an earlier worker held because its Prepare got no grant is released by the new one:
    # those workers held the slot for good even when the server had answered with a refusal.
    state = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "agent-q/service" / config.name
    released = []
    for held in sorted([*state.glob("slots/*/held"), *state.glob("nodes/*/slots/*/held")]):
        current = held.parent / "current"
        if not held.read_text().startswith("prepare_disposition_unknown"):
            continue
        if current.is_dir() and not (current / "server-refused").exists():
            atomic_text(current / "server-refused", "released by a remote worker update")
            released.append(held.parent.name)
    return released


def worker_binary_update(config, installer, origin):
    """Replaces the installed aq-worker with the one in the bundle the pinned installer names, and
    restarts the worker service, so a machine takes a worker fix without anyone running the
    installer on it. The bundle is checked against the checksum the installer pins. Only the
    worker binary is replaced; the bundle's other programs change with a full install.
    """
    pinned = re.search(r"^digest='([a-f0-9]{64})'$", installer, re.M)
    if not pinned:
        raise ValueError("The pinned installer names no worker bundle.")
    runtime = Path.home() / ".local/share/agent-q/runtime"
    worker = runtime / "aq-worker"
    if not worker.is_file() or worker.is_symlink():
        raise ValueError("No installed worker to update.")
    with tempfile.TemporaryDirectory(dir=runtime) as work:
        archive, fresh = Path(work) / "bundle.tar.gz", Path(work) / "aq-worker"
        download_worker_bundle(origin, pinned.group(1), archive)
        extract_worker_binary(archive, fresh)
        fresh.chmod(0o755)
        version = subprocess.run([str(fresh), "--version"], check=True, timeout=20, capture_output=True, text=True).stdout.strip()
        os.replace(fresh, worker)
    released = release_refused_slots(config)
    subprocess.run(["systemctl", "--user", "restart", "--no-block", "aq-node@" + config.name + ".service"], check=True, timeout=20, capture_output=True)
    return {"version": version, "bundle_sha256": pinned.group(1), "restarted": True, "released_slots": released}


def session_state_file(directory):
    state = directory / "state.json"
    return directory.is_dir() and not directory.is_symlink() and state.is_file() and not state.is_symlink()


def known_value(value, choices):
    return value if value in choices else "unknown"


def session_summary(path):
    value = json.loads(path.read_text())
    return {
        "id": str(uuid.UUID(value["id"])),
        "status": known_value(
            value.get("status"), ("pending", "preparing", "running", "stopping", "stopped", "failed")
        ),
        "sandbox_created": isinstance(value.get("sandbox"), dict),
        "provider": known_value(value.get("spec", {}).get("provider"), ("anthropic", "openai")),
    }


def recent_sessions(state_root):
    recent = []
    if not state_root.is_dir():
        return recent
    paths = (p / "state.json" for p in state_root.iterdir() if session_state_file(p))
    for path in sorted(paths, key=lambda p: p.stat().st_mtime, reverse=True)[:4]:
        if path.stat().st_size <= 65536:
            try:
                recent.append(session_summary(path))
            except (ValueError, OSError, KeyError, TypeError):
                continue
    return recent


def sandbox_client(socket_path):
    class Connection(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.settimeout(20)
            self.sock.connect(str(socket_path))

    def call(method, path, body=None):
        connection = Connection("localhost", timeout=20)
        try:
            connection.request(
                method,
                "/v1.44" + path,
                body=None if body is None else json.dumps(body).encode(),
                headers={} if body is None else {"Content-Type": "application/json"},
            )
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

    return call, expected


def shares_root_filesystem(storage):
    return not storage or not Path(storage).exists() or Path(storage).stat().st_dev == Path("/").stat().st_dev


def check_sandbox_info(expected, details):
    raw = expected("GET", "/info", None, (200,))
    info = json.loads(raw)
    for key in ("CgroupDriver", "CgroupVersion", "SecurityOptions", "DockerRootDir"):
        details[key] = info.get(key)
    if not rootless_daemon(info):
        raise ValueError("The sandbox daemon did not report rootless mode.")
    if not cgroup_v2(info):
        raise ValueError("The worker requires a cgroup v2 resource controller.")
    shared = shares_root_filesystem(info.get("DockerRootDir"))
    details["storage_shares_root"] = shared
    if shared and not details["shared_root_fs"]:
        raise ValueError("Sandbox storage shares the root filesystem but this installation has no shared_root_fs override.")


def probe_container(image, volume):
    script = "set -eu; git --version; tmux -S /tmp/aq-check.sock new-session -d -s check 'sleep 10'; tmux -S /tmp/aq-check.sock has-session -t check; tmux -S /tmp/aq-check.sock kill-server"
    return {
        "Image": image,
        "Labels": {"aq.install-check": "true"},
        "Env": [],
        "WorkingDir": "/aq/work",
        "ExposedPorts": {},
        "AttachStdin": False,
        "OpenStdin": False,
        "Tty": False,
        "Cmd": ["/bin/bash", "-c", script],
        "HostConfig": {
            "Privileged": False,
            "CapAdd": [],
            "CapDrop": ["ALL"],
            "Devices": [],
            "DeviceCgroupRules": [],
            "SecurityOpt": ["no-new-privileges"],
            "NanoCpus": 2000000000,
            "Memory": 4294967296,
            "MemorySwap": 4294967296,
            "PidsLimit": 512,
            "NetworkMode": "bridge",
            "PortBindings": {},
            "PublishAllPorts": False,
            "Binds": [],
            "Mounts": [{"Type": "volume", "Source": volume, "Target": "/aq/work", "ReadOnly": False}],
            "IpcMode": "private",
            "RestartPolicy": {"Name": "no"},
            "AutoRemove": False,
        },
    }


def is_log_frame(raw):
    return len(raw) >= 8 and raw[0] in (0, 1, 2) and raw[1:4] == b"\0\0\0"


def probe_output(raw):
    # Docker log frames contain fixed-probe output only, never an agent session.
    logs = bytearray()
    while is_log_frame(raw):
        size = int.from_bytes(raw[4:8], "big")
        logs.extend(raw[8:8+size])
        raw = raw[8+size:]
    logs.extend(raw)
    return logs.decode("utf-8", errors="replace")[-4096:]


def await_probe_exit(call, expected, probe):
    import time
    for _ in range(30):
        container = json.loads(expected("GET", "/containers/" + probe + "/json", None, (200,)))
        state = container.get("State", {})
        if state.get("Status") in ("exited", "dead"):
            if state.get("ExitCode") == 0:
                return
            _, raw = call("GET", "/containers/" + probe + "/logs?stdout=1&stderr=1")
            raise ValueError("The fixed sandbox check exited with code " + str(state.get("ExitCode")) + ": " + probe_output(raw))
        time.sleep(1)
    raise ValueError("The fixed sandbox check has not exited.")


def cleanup_probe(call, probe, made_container, made_volume):
    try:
        if made_container:
            call("DELETE", "/containers/" + probe + "?force=true")
        if made_volume:
            call("DELETE", "/volumes/" + probe + "-work")
    except (OSError, ValueError, http.client.HTTPException):
        pass




def sandbox_probe(config):
    """Probe only AQ's configured sandbox; never read provider or node credentials."""
    service = read_toml(config / "service.toml")
    native = json.loads(Path(service["native_template"]).read_text())
    docker = native["executor"]["settings"]["host"]["docker_local"]
    socket_path = Path(docker["socket"])
    system_sockets = (Path("/var/run/docker.sock"), Path("/run/docker.sock"), Path("/run/podman/podman.sock"))
    if not socket_path.is_absolute() or socket_path.resolve() in system_sockets or socket_path.stat().st_uid != os.getuid():
        raise ValueError("The AQ sandbox socket must belong to the worker user.")
    details = {"socket": str(socket_path), "shared_root_fs": docker.get("shared_root_fs", False)}
    details["recent_sessions"] = recent_sessions(Path(service["state_root"]) / "manual-agents")
    call, expected = sandbox_client(socket_path)
    phase = "daemon_info"
    probe = "aq-worker-check-" + uuid.uuid4().hex[:12]
    volume = probe + "-work"
    made_volume = made_container = False
    try:
        check_sandbox_info(expected, details)
        image, = docker["templates"]
        details["image"] = image["image"]
        phase = "image"
        from urllib.parse import quote
        expected("GET", "/images/" + quote(image["image"], safe="") + "/json", None, (200,))
        phase = "volume_create"
        expected("POST", "/volumes/create", {"Name": volume, "Driver": "local", "Labels": {"aq.install-check": "true"}}, (200, 201))
        made_volume = True
        phase = "container_create"
        body = probe_container(image["image"], volume)
        expected("POST", "/containers/create?name=" + probe, body, (201,))
        made_container = True
        phase = "container_start"
        expected("POST", "/containers/" + probe + "/start", None, (204, 304))
        phase = "tmux"
        await_probe_exit(call, expected, probe)
        return {"ok": True, "phase": "complete", "details": details}
    except (ValueError, OSError, KeyError, TypeError, http.client.HTTPException) as error:
        # No credentials are read or mounted by this probe. These errors describe
        # only its own daemon request, filesystem metadata, or fixed command.
        return {"ok": False, "phase": phase, "error": str(error)[:4096], "details": details}
    finally:
        cleanup_probe(call, probe, made_container, made_volume)


def owned_directory(path):
    return path.is_dir() and not path.is_symlink() and path.stat().st_uid == os.getuid()


def valid_network_archive(members):
    return (
        len(members) <= 512
        and sum(m.size for m in members) <= 33554432
        and all(m.isfile() or m.isdir() for m in members)
    )


def network_runtime(config):
    state = read_toml(config / "service.toml")
    prefix = Path.home() / ".local/opt/aq-sandbox-podman"
    expected = prefix / "podman-linux-amd64/usr/local/bin/podman"
    configured = Path(state.get("manual", {}).get("podman_executable", "/unconfigured"))
    if configured != expected or not owned_directory(prefix):
        raise ValueError("Network helper repair is restricted to the installed AQ sandbox runtime.")
    kill_mode = subprocess.run(
        ["systemctl", "--user", "show", "aq-sandbox-podman.service", "--property=KillMode", "--value"],
        check=True,
        capture_output=True,
        text=True,
        timeout=15,
    ).stdout.strip()
    if kill_mode != "process":
        raise ValueError("The sandbox service must preserve running containers before it can be restarted.")
    return prefix


def fetch_network_tools(origin):
    with urllib.request.build_opener(NoRedirect()).open(origin + "/install/" + NETWORK_TOOLS_SHA256 + ".tar.gz", timeout=30) as response:
        contents = response.read(8388609)
    if len(contents) > 8388608 or hashlib.sha256(contents).hexdigest() != NETWORK_TOOLS_SHA256:
        raise ValueError("AQ network tools failed checksum verification.")
    return contents


def extract_network_tools(contents, target):
    with tempfile.TemporaryDirectory(prefix="stage-", dir=target.parent) as stage:
        stage = Path(stage)
        archive = stage / "tools.tar.gz"
        archive.write_bytes(contents)
        unpacked = stage / "runtime"
        unpacked.mkdir()
        with tarfile.open(archive) as tar:
            members = tar.getmembers()
            if not valid_network_archive(members):
                raise ValueError("AQ network tools archive was not a supported runtime.")
            tar.extractall(unpacked, filter="data")
        if not (unpacked / "bin/iptables").is_file() or not (unpacked / "manifest.json").is_file():
            raise ValueError("AQ network tools archive is incomplete.")
        unpacked.rename(target)


def verified_manifest_file(root, path, digest):
    return path.is_relative_to(root) and path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest() == digest


def verify_network_tools(target):
    manifest = json.loads((target / "manifest.json").read_text())
    root = target.resolve()
    for name, fact in manifest["files"].items():
        path = (target / name).resolve()
        if not verified_manifest_file(root, path, fact["sha256"]):
            raise ValueError("Installed AQ network tools do not match their manifest.")


def activate_network_tools(target):
    unitroot = (
        Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
        / "systemd/user/aq-sandbox-podman.service.d"
    )
    unitroot.mkdir(parents=True, exist_ok=True)
    path = str(target / "bin") + ":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    path = path.replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"')
    atomic_text(unitroot / "30-aq-network-tools.conf", '[Service]\nEnvironment="PATH=' + path + '"\n')
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True, capture_output=True, timeout=15)
    subprocess.run(
        ["systemctl", "--user", "restart", "aq-sandbox-podman.service"],
        check=True,
        capture_output=True,
        timeout=30,
    )


def install_network_tools(config):
    """Restore an AQ-owned runtime dependency without sudo or system packages."""
    prefix = network_runtime(config)
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]
    if not re.fullmatch(r"https://[a-zA-Z0-9][a-zA-Z0-9.-]*(?::[0-9]{1,5})?", origin):
        raise ValueError("Network tools require the installed HTTPS worker origin.")
    root = prefix / "network-tools"
    root.mkdir(mode=0o700, exist_ok=True)
    target = root / NETWORK_TOOLS_SHA256
    if not target.exists():
        extract_network_tools(fetch_network_tools(origin), target)
    verify_network_tools(target)
    activate_network_tools(target)
    return NETWORK_TOOLS_SHA256


def retry_repaired_sandbox(config):
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
    return retry


def sandbox_diagnostics(config):
    report = sandbox_probe(config)
    missing = not report.get("ok") and report.get("phase") == "container_start" and "iptables" in report.get("error", "") and "No such file or directory" in report.get("error", "")
    if not missing:
        return report
    try:
        restored = install_network_tools(config)
        retry = retry_repaired_sandbox(config)
        retry.setdefault("details", {})["network_tools_restored"] = restored
        return retry
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        message = str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "AQ's private network helper repair failed."
        return {"ok": False, "phase": "network_support", "error": message, "details": report.get("details", {})}

def owned_file(path):
    return path.is_file() and not path.is_symlink() and path.stat().st_uid == os.getuid()


def needs_private_mode(path):
    return owned_file(path) and path.stat().st_mode & 0o7777 != 0o600


def repair_native_files(config):
    """Keeps what the worker requires of its template true, so a machine installed by an earlier
    installer mends itself: the worker reads its adapter, composite and inventory definitions only
    as private files (0600), and needs its private state directory (0700). A bundle unpacks the
    definitions as 0644, and nothing created the directory. Only this user's regular files are touched.
    """
    native = json.loads(Path(read_toml(config / "service.toml")["native_template"]).read_text())
    for value in (native.get("adapter_manifest"), native.get("inventory_definition"), native["executor"].get("composite_definition")):
        path = Path(str(value))
        if path.is_absolute() and needs_private_mode(path):
            path.chmod(0o600)
    state = Path(str(native.get("state_dir")))
    if state.is_absolute() and state.parent == config:
        state.mkdir(mode=0o700, exist_ok=True)
        state.chmod(0o700)


def worker_service_report(config):
    """What the worker service says about itself, for a remote check: whether it runs, its version,
    which of its template's paths are missing, and its last log lines. It reads no credential; a
    token-shaped string in a log line is replaced. Kept small: the whole report is at most 16 KiB.
    """
    unit = "aq-node@" + config.name + ".service"
    def run(*argv):
        try:
            return subprocess.run(argv, capture_output=True, text=True, timeout=15).stdout
        except (OSError, subprocess.SubprocessError):
            return ""
    report = {
        "active": run("systemctl", "--user", "is-active", unit).strip()[:32],
        "version": run(str(Path.home() / ".local/share/agent-q/runtime/aq-worker"), "--version").strip()[:96],
    }
    try:
        service = read_toml(config / "service.toml")
        native = json.loads(Path(service["native_template"]).read_text())
        host = native["executor"]["settings"]["host"]
        paths = {
            "state_dir": native.get("state_dir"), "adapter_manifest": native.get("adapter_manifest"),
            "inventory_definition": native.get("inventory_definition"),
            "composite_definition": native["executor"].get("composite_definition"),
            "socket": host["docker_local"]["socket"],
            "helper": host["helper"]["path"], "tmux": host["tmux"]["path"],
        }
        report["missing"] = sorted(key for key, value in paths.items() if not (isinstance(value, str) and os.path.exists(value)))
        state = Path(str(paths["state_dir"]))
        report["state_dir"] = {"mode": oct(state.stat().st_mode & 0o7777), "holds_git_root": str(native.get("git_root", "")).startswith(str(state) + "/")} if state.is_dir() else None
    except (OSError, ValueError, KeyError, TypeError) as error:
        report["template_error"] = str(error)[:300]
    secret = re.compile(r"sk-ant-[A-Za-z0-9_-]+|eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_.-]+|Bearer\s+\S+")
    lines = run("journalctl", "--user", "-u", unit, "-n", "40", "--no-pager", "-o", "cat").splitlines()
    # A refusal carries the server's reason at the end of a long line: keep those lines whole enough to read it.
    report["log"] = [secret.sub("[redacted]", line)[:900 if "refused" in line else 240] for line in lines][-24:]
    return report


def run_diagnostic(config, job):
    try:
        if job.get("kind") == "sandbox_startup_v1":
            report = sandbox_diagnostics(config)
            report.setdefault("details", {})["worker"] = worker_service_report(config)
        elif job.get("kind") == "receiver_update_v1":
            report = receiver_update(config, job.get("installer_sha256"))
        else:
            raise ValueError("This receiver does not support the requested check.")
    except (ValueError, OSError, KeyError, TypeError) as error:
        report = {"ok": False, "phase": "receiver", "error": str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "The receiver could not complete its fixed check."}
    return report


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
        report = run_diagnostic(config, job)
        atomic_text(receipt, json.dumps({"operation_id": operation, "report": report}))
    diagnostic_request(config, {"operation_id": operation, "report": report}, slot)


def supported_login_material(material):
    return isinstance(material, str) and 0 < len(material.encode()) <= 65536


def validated_claude_login(logins):
    if not isinstance(logins, list) or len(logins) != 1:
        raise ValueError("Invalid login delivery.")
    provider, material = logins[0]["provider"], logins[0]["material"]
    if provider != "anthropic" or not supported_login_material(material):
        raise ValueError("Invalid login delivery.")
    if not all(33 <= ord(c) <= 126 for c in material):
        raise ValueError("Invalid Claude subscription login.")
    return material


def install_claude_login(config, material):
    private = config / "provider-auth"
    if private.is_symlink():
        raise ValueError("The private login directory must not be a link.")
    private.mkdir(mode=0o700, exist_ok=True)
    private.chmod(0o700)
    target = private / "claude-token"
    new_provider = not target.exists()
    atomic_text(target, material)
    return target, new_provider


def configure_login_path(package, config, claude):
    service = config / "service.toml"
    previous = service.read_text()
    contents = previous
    current = tomllib.loads(contents)
    metadata = json.loads((package / "bootstrap.json").read_text())
    if "manual" not in current:
        settings = json.loads((config / "login-sync.json").read_text())
        contents = contents.rstrip() + manual_settings(package, settings["podman"], node_count(current), metadata["source_slugs"], claude)
    else:
        contents = set_toml_value(contents, "manual", "claude_token_file", claude)
    if contents != previous:
        atomic_text(service, contents)
    return contents != previous


def apply_logins(package, config, logins):
    """Installs the Claude token file. A Codex login is never delivered to a machine: each
    Codex agent leases AQ's saved login from the server and writes a refreshed one back."""
    material = validated_claude_login(logins)
    target, new_provider = install_claude_login(config, material)
    changed = configure_login_path(package, config, str(target))
    return changed or new_provider


def enrolled_slots(config):
    state = None
    try:
        state = read_toml(config / "service.toml")
        count = node_count(state)
        if type(count) is not int or not 1 <= count <= 8:
            raise ValueError("Invalid node count")
        slots = list(range(1, count + 1))
    except (ValueError, OSError, KeyError):
        # Control traffic must survive the broken configuration it diagnoses.
        slots = [slot for slot in range(1, 9) if (config / f"slots/{slot}/worker.toml").is_file()]
    return state, slots


def poll_slots(package, config, slots):
    failed = False
    for slot in slots:
        for receive in (sync_slot, sync_diagnostics_slot):
            try:
                receive(package, config, slot)
            except (OSError, ValueError, KeyError, subprocess.SubprocessError, http.client.HTTPException):
                # One failing function/node must not prevent other requests.
                failed = True
    return failed


def sync_logins(package, config):
    state, slots = enrolled_slots(config)
    try:
        repair_native_files(config)
    except (OSError, ValueError, KeyError, TypeError):
        pass
    failed = poll_slots(package, config, slots)
    # The worker process owns agent lifetime; its separate relay owns disposable
    # terminal attachments. Hosted installs must start both, including workers
    # whose manual settings arrived later through Push Claude token.
    if state is not None:
        try:
            ensure_terminal_relay(package, config)
            ensure_host_terminal(config)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError):
            failed = True
    if failed:
        raise ValueError("Worker controls are waiting or need attention. Check this worker in the dashboard.")


def allowed_companion(member):
    return member.isfile() and 0 < member.size <= 268435456


def host_bundle_files(archive, destination):
    """Extract only regular, bounded companion files from a verified bundle."""
    allowed = {"aq-worker", "tmux", "tmux-libs/libtinfo.so.6"}
    found = set()
    with tarfile.open(archive, "r:gz") as bundle:
        for member in bundle:
            if member.name not in allowed:
                continue
            if member.name in found or not allowed_companion(member):
                raise ValueError("Invalid host terminal companion file.")
            found.add(member.name)
            target = destination / member.name
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            with bundle.extractfile(member) as source, target.open("xb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(0o700 if member.name in {"aq-worker", "tmux"} else 0o600)
    if not {"aq-worker", "tmux"}.issubset(found):
        raise ValueError("Host terminal companion is missing from the pinned bundle.")


def enable_relay(target, unit, name):
    changed = not target.exists() or target.read_text() != unit
    if changed:
        atomic_text(target, unit)
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=True, capture_output=True, timeout=15)
    active = (
        subprocess.run(
            ["systemctl", "--user", "is-active", "--quiet", name], capture_output=True, timeout=15
        ).returncode
        == 0
    )
    if changed or not active:
        subprocess.run(["systemctl", "--user", "enable", name], check=True, capture_output=True, timeout=15)
        subprocess.run(
            ["systemctl", "--user", "restart" if changed else "start", name],
            check=True,
            capture_output=True,
            timeout=20,
        )


def checked_profile(profile):
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,31}", profile):
        raise ValueError("Invalid worker profile.")
    return profile


def plain_https_origin(origin):
    parsed = urlsplit(origin)
    return (
        parsed.scheme == "https"
        and parsed.path in ("", "/")
        and not any((parsed.username, parsed.password, parsed.query, parsed.fragment))
    )


def enrolled_host_origin(config):
    marker = json.loads((config / "hosted-install.json").read_text())
    origin = marker["origin"]
    if not plain_https_origin(origin):
        raise ValueError("Host terminal bundle needs the enrolled HTTPS origin.")
    enrollment = read_toml(config / "slots/1/worker.toml")
    if enrollment["server_url"].rstrip("/") != origin.rstrip("/"):
        raise ValueError("Host terminal bundle origin differs from the enrollment.")
    return origin


def download_host_bundle(origin, digest, archive):
    checksum, size = hashlib.sha256(), 0
    with urllib.request.build_opener(NoRedirect()).open(origin.rstrip("/") + "/install/" + digest + ".tar.gz", timeout=30) as response, archive.open("xb") as output:
        while chunk := response.read(1048576):
            size += len(chunk)
            if size > 536870912:
                raise ValueError("Host terminal bundle exceeded its bound.")
            checksum.update(chunk)
            output.write(chunk)
    if checksum.hexdigest() != digest:
        raise ValueError("Host terminal bundle checksum did not match.")


def host_terminal_runtime(state, origin, digest):
    parent = Path(state["state_root"]) / "host-terminal-runtime"
    parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    package = parent / digest
    if not package.is_dir():
        with tempfile.TemporaryDirectory(dir=parent) as temporary:
            temporary = Path(temporary)
            archive = temporary / "bundle.tar.gz"
            download_host_bundle(origin, digest, archive)
            extracted = temporary / "runtime"
            extracted.mkdir(mode=0o700)
            host_bundle_files(archive, extracted)
            extracted.rename(package)
    return package


def systemd_argument(value):
    return '"' + str(value).replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"') + '"'


def ensure_host_terminal(config):
    digest = HOST_BUNDLE_SHA256
    if not re.fullmatch(r"[a-f0-9]{64}", digest):
        return  # source/unpackaged receiver, including local fixtures
    state = read_toml(config / "service.toml")
    profile = checked_profile(state["profile"])
    origin = enrolled_host_origin(config)
    package = host_terminal_runtime(state, origin, digest)
    root = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    root.mkdir(parents=True, exist_ok=True)
    command = " ".join(systemd_argument(value) for value in (package / "aq-worker", "host-terminal-relay", "--service-config", config / "service.toml"))
    name = "aq-host-terminal-" + profile + ".service"
    target = root / name
    unit = "[Unit]\nDescription=AQ worker host terminal (" + profile + ")\n[Service]\nType=simple\nUMask=0077\nExecStart=" + command + "\nRestart=always\nRestartSec=2\nTimeoutStopSec=15\nKillMode=mixed\n[Install]\nWantedBy=default.target\n"
    enable_relay(target, unit, name)


def ensure_terminal_relay(package, config):
    state = read_toml(config / "service.toml")
    if not state.get("manual"):
        return
    profile = checked_profile(state["profile"])
    worker = package / "aq-worker"
    if not worker.is_file() or not os.access(worker, os.X_OK):
        raise ValueError("The installed worker terminal runtime is missing.")
    (Path(state["state_root"]) / "manual-agents").mkdir(parents=True, mode=0o700, exist_ok=True)
    root = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    root.mkdir(parents=True, exist_ok=True)
    name = "aq-terminal-relay-" + profile + ".service"
    target = root / name
    command = " ".join(systemd_argument(value) for value in (worker, "terminal-relay", "--service-config", config / "service.toml"))
    unit = "[Unit]\nDescription=AQ interactive terminal transport (" + profile + ")\nAfter=aq-node@" + profile + ".service\n[Service]\nType=simple\nUMask=0077\nExecStart=" + command + "\nRestart=always\nRestartSec=2\nTimeoutStopSec=10\nKillMode=mixed\n[Install]\nWantedBy=default.target\n"
    enable_relay(target, unit, name)


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
            raise ValueError("Login installation failed. Check the worker configuration and retry Push Claude token.") from None
    login_request(config, {"generation": generation, "applied": True}, slot)



def report_upgrade_failure(config, message):
    try:
        job = diagnostic_request(config, {})
        if job.get("operation_id"):
            diagnostic_request(config, {"operation_id": job["operation_id"], "report": {"ok": False, "phase": "controls_upgrade", "error": message}})
    except (ValueError, OSError, KeyError):
        pass


def upgrade_controls(config, origin):
    """Upgrade an already connected receiver without reinstalling its runtime."""
    profile = checked_profile(config.name)
    try:
        marker = json.loads((config / "hosted-install.json").read_text())
        slot = read_toml(config / "slots/1/worker.toml")
        if marker["origin"] != origin or slot["server_url"] != origin:
            raise ValueError("This worker belongs to another server.")
        target = config / "login-sync.py"
        if not owned_file(target):
            raise ValueError("The installed AQ control receiver was not found for this profile.")
        atomic_text(target, Path(__file__).read_text())
        subprocess.run(["systemctl", "--user", "restart", "aq-login-sync-" + profile + ".timer"], check=True, timeout=20, capture_output=True)
        subprocess.run(["systemctl", "--user", "start", "--no-block", "aq-login-sync-" + profile + ".service"], check=True, timeout=20, capture_output=True)
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        # Bootstrap may fail before its timer is running. Return its own fixed
        # setup diagnostic over the existing enrollment whenever possible.
        message = str(error)[:4096] if type(error) is ValueError or isinstance(error, OSError) else "The AQ control service could not be started."
        report_upgrade_failure(config, message)
        raise ValueError(message) from None
    print("Worker controls updated. AQ will collect the queued diagnostic directly.")

def install_sync(package, config):
    state = read_toml(config / "service.toml")
    profile = checked_profile(state["profile"])
    settings = {"podman": str(Path.home() / ".local/opt/aq-sandbox-podman/podman-linux-amd64/usr/local/bin/podman")}
    atomic_text(config / "login-sync.json", json.dumps(settings))
    helper = config / "login-sync.py"
    if Path(__file__).resolve() != helper.resolve():
        atomic_text(helper, Path(__file__).read_text())
    unit_root = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    unit_root.mkdir(parents=True, exist_ok=True)
    # systemd specifiers and quoting are escaped independently of shell syntax.
    command = " ".join(systemd_argument(v) for v in (sys.executable, helper, "sync", package, config))
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
        config, origin, podman, slots, digest, *claude_token = args
        configure(package, Path(config), origin, Path(podman), int(slots), digest, *map(Path, claude_token[:1]))
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
