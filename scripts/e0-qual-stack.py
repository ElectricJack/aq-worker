#!/usr/bin/env python3
"""Separate, reproducible E0 qualification stack (Compose project aq-e0-qual).

Consumes the published E0 images and the unmodified E0 source checkout as a
read-only Compose project directory. Everything this script writes lives under
its own state root and its own fresh named volumes. It never edits E0 source,
never writes an aq-e0-* volume and never admits anything itself: every
registration, catalog and binding change goes through the public API.

Commands run in order: prepare, up, enroll, seed, measure, configure, apply,
status. `down` stops the project (volumes are kept unless --volumes is given).
Secrets are never printed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

HERE = Path(__file__).resolve().parent
QUAL = HERE / "e0-qual"
PINS = json.loads((QUAL / "pins.json").read_text())
PROJECT_NAME = PINS["compose_project"]
# Fresh identifiers distinct from the E0 dev seed (e0000000-...).
WORKSPACE = "e0a10000-0000-4000-8000-000000000001"
PROJECT = "e0a10000-0000-4000-8000-000000000002"
REPOSITORY = "e0a10000-0000-4000-8000-000000000003"
TASK = "e0a10000-0000-4000-8000-000000000004"
OP_REGISTER = "e0a10000-0000-4000-8000-000000000006"
OP_CATALOG_1 = "e0a10000-0000-4000-8000-000000000007"
OP_BINDING_1 = "e0a10000-0000-4000-8000-000000000008"
OP_CATALOG_2 = "e0a10000-0000-4000-8000-000000000017"
OP_BINDING_2 = "e0a10000-0000-4000-8000-000000000018"
OP_SYNC = "e0a10000-0000-4000-8000-000000000009"
OP_WORKSPACE = "e0a10000-0000-4000-8000-000000000010"
OP_CHECKPOINT = "e0a10000-0000-4000-8000-000000000011"
OP_RETIRE = "e0a10000-0000-4000-8000-000000000012"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def root():
    return Path(os.environ.get("AQ_QUAL_ROOT", "~/.local/state/aq-platform-e0-qual")).expanduser().resolve()


def e0_source():
    path = Path(os.environ.get("AQ_E0_SOURCE", PINS["e0_source"]["path"])).expanduser().resolve()
    require((path / "compose.dev.yaml").is_file() and (path / "scripts/dev-catalog.json").is_file(),
            "E0 source checkout not found; set AQ_E0_SOURCE")
    return path


def layout():
    base = root()
    return dict(root=base, state=base / "state", secret=base / "secrets", vault=base / "state/vault",
                qual=base / "qual", evidence=base / "evidence")


def private_bytes(path, content, replace=False):
    """Publish an owner-only file atomically; never print its bytes."""
    try:
        metadata = path.lstat()
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1 and metadata.st_uid == os.getuid()
                and not metadata.st_mode & 0o077, f"unsafe retained private file {path.name}")
        if not replace:
            require(path.read_bytes() == content, f"retained {path.name} differs; use a fresh AQ_QUAL_ROOT")
            return
    except FileNotFoundError:
        pass
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".qual-", delete=False) as out:
        temporary = Path(out.name)
        os.fchmod(out.fileno(), 0o600)
        out.write(content)
        out.flush()
        os.fsync(out.fileno())
    os.replace(temporary, path)


def env():
    p = layout()
    ports = PINS["ports"]
    images = PINS["images"]
    return dict(
        COMPOSE_PROJECT_NAME=PROJECT_NAME,
        AQ_DEV_STATE_DIR=str(p["state"]), AQ_VAULT_DIR=str(p["vault"]), AQ_SECRETS_DIR=str(p["secret"]),
        AQ_UPDATER_SPOOL=str(p["state"] / "updater-spool"), AQ_DEV_UID=str(os.getuid()), AQ_DEV_GID=str(os.getgid()),
        AQ_DEV_GRAPH_WORKSPACE=WORKSPACE, AQ_GRAPH_WORKSPACE=WORKSPACE,
        AQ_SERVER_IMAGE=images["server_support"], AQ_QUAL_SERVER_IMAGE=images["server"],
        AQ_DEV_GRAPH_IMAGE=images["graph"],
        AQ_DEV_GRAPH_PORT=str(ports["graph"]), AQ_DEV_ADMIN_PORT=str(ports["admin"]),
        AQ_DEV_WORKER_PORT=str(ports["worker"]),
        AQ_QUAL_DIR=str(p["qual"]), AQ_QUAL_INIT_SCRIPT=str(QUAL / "qual-vault-init.sh"))


def urls():
    ports = PINS["ports"]
    return (f"http://127.0.0.1:{ports['graph']}", f"http://127.0.0.1:{ports['admin']}",
            f"http://127.0.0.1:{ports['worker']}")


def prepare():
    p = layout()
    for directory in (p["root"], p["state"], p["secret"], p["vault"], p["qual"], p["qual"] / "secrets",
                      p["evidence"], p["state"] / "updater-spool/requests", p["state"] / "updater-spool/status"):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        directory.chmod(0o700)
    (p["state"] / "fixture").mkdir(exist_ok=True, mode=0o755)
    (p["state"] / "fixture").chmod(0o755)
    secret = p["secret"]
    # Same secret shapes as the E0 dev stack, generated fresh for this project.
    for name, mode in (("postgres_password", 0o644), ("graph_database_password", 0o644), ("graph_token", 0o600)):
        path = secret / name
        try:
            with path.open("x") as out:
                out.write(secrets.token_hex(32))
        except FileExistsError:
            require(not path.is_symlink() and path.is_file() and path.stat().st_size >= 32,
                    "invalid retained qualification credential file")
        path.chmod(mode)
    for name, factory in (("playbook_database_password", lambda: secrets.token_hex(32).encode()),
                          ("playbook_engine_keys", lambda: json.dumps(dict(active_key_id="dev-engine-1",
                              keys={"dev-engine-1": secrets.token_hex(32)})).encode()),
                          ("playbook_bridge_key", lambda: secrets.token_bytes(32))):
        if not (secret / name).exists():
            private_bytes(secret / name, factory())
    target = p["vault"] / "config.toml"
    config = base_config()
    require(not target.exists() or target.read_text() == config,
            "qualification base vault differs; use a fresh AQ_QUAL_ROOT")
    target.write_text(config)
    return env()


def base_config():
    config = (e0_source() / "vault.example/config.toml").read_text()
    admin = PINS["ports"]["admin"]
    config = config.replace('public_origin = "http://127.0.0.1:8080"', f'public_origin = "http://127.0.0.1:{admin}"')
    return config + '''
[agent_access]
graph_url = "http://aq-graph:8741"
graph_token_file = "/run/aq-dev/graph_token"
mutation_actor_id = "dev-service"
[source_retention]
staging_root = "/var/lib/aq/source-staging"
git_root = "/var/lib/aq/git"
git_socket = "/run/aq-git/central.sock"
'''


def compose(*args, capture=True, input=None, timeout=None, check=True):
    source = e0_source()
    command = ["docker", "compose", "-p", PROJECT_NAME, "--project-directory", str(source),
               "-f", str(source / "compose.yaml"), "-f", str(source / "compose.dev.yaml"),
               "-f", str(QUAL / "compose.qual.yaml"), *args]
    result = subprocess.run(command, cwd=source, env=dict(os.environ, **env()), text=True,
                            capture_output=capture, input=input, timeout=timeout)
    if result.returncode and not check:
        return result.stdout
    if result.returncode:
        raise RuntimeError(f"compose {' '.join(args[:2])} exited {result.returncode}: "
                           f"{(result.stderr or '')[-2000:]}" if capture else "compose command failed")
    return result.stdout if capture else None


def http(url, body=None, token=None, allow_missing=True):
    headers = {"Content-Type": "application/json", "X-AQ-Request": "1"}
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as error:
        if error.code == 404 and allow_missing:
            return None
        detail = ""
        try:
            parsed = json.loads(error.read()[:4096])
            # Public error codes only; bodies never carry credentials here.
            detail = " " + json.dumps({k: parsed[k] for k in ("code", "error", "message") if k in parsed})
        except Exception:
            pass
        raise RuntimeError(f"public API {url.split('127.0.0.1')[-1]} returned HTTP {error.code}{detail}") from None


def git_request(operation, params=None, mutation=None, generation=1):
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation,
                   params=params or {}, deadline_ms=10000)
    if operation != "capabilities":
        request.update(repository_id=REPOSITORY,
                       expected_binding_generation=0 if operation == "repo.register" else generation)
    if mutation:
        request["operation_id"] = mutation
    raw = compose("exec", "-T", "aq-git", "aq-git", "--socket", "/run/aq-git/central.sock",
                  "--input", "-", "--json", "request", input=json.dumps(request) + "\n", timeout=120, check=False)
    require(len(raw.encode()) <= 65536 and raw.endswith("\n"), "invalid bounded aq-git response")
    response = json.loads(raw)
    if response.get("request_id") != request["request_id"] or response.get("ok") is not True:
        raise RuntimeError(f"aq-git refused {operation}: {json.dumps(response.get('error'))[:500]}")
    return response["result"]


def receipt(name, value=None):
    path = layout()["state"] / name
    if value is None:
        return json.loads(path.read_text()) if path.exists() else None
    path.write_text(json.dumps(value, sort_keys=True, indent=1) + "\n")
    return value


def up():
    prepare()
    for port in PINS["ports"].values():
        probe = subprocess.run(["ss", "-ltnH", f"sport = :{port}"], capture_output=True, text=True)
        running = compose("ps", "-q", "server").strip()
        require(not probe.stdout.strip() or running, f"port {port} is already in use by another stack")
    compose("up", "-d", "--no-build", "--wait", "--wait-timeout", "300", "server", capture=False)
    return status()


def status():
    _, server, _ = urls()
    workers = http(server + "/api/workers") or []
    catalogs = http(server + "/api/profiles/catalogs")
    project = http(server + "/api/projects/" + PROJECT)
    return dict(project=PROJECT_NAME, workers=[{k: w.get(k) for k in ("id", "name", "status")} for w in workers],
                catalog=None if catalogs is None else {k: catalogs.get(k) for k in ("active_digest", "revision")},
                binding_revision=None if project is None else project.get("revision"))


def verify_worker_credential(record, node_id):
    _, _, worker_url = urls()
    require(isinstance(record, dict) and record.get("node_id") == node_id
            and isinstance(record.get("credential"), str) and record["credential"].startswith("aqn_"),
            "saved qualification worker credential is incomplete or belongs to another node")
    nonce = uuid.uuid4().hex
    reply = http(worker_url + "/v1/worker/probe", dict(nonce=nonce), record["credential"], allow_missing=False)
    require(isinstance(reply, dict) and reply.get("nonce") == nonce, "worker credential probe not correlated")


def enroll():
    """Enroll a fresh desktop node through the ordinary public enrollment route."""
    p = layout()
    _, server, worker_url = urls()
    name = PINS["node_name"]
    credential_file = p["secret"] / "qual_worker.json"
    workers = [w for w in http(server + "/api/workers") or [] if w["name"] == name]
    require(len(workers) <= 1 and not any(w["status"] == "revoked" for w in workers),
            "qualification worker identity is ambiguous or revoked")
    if workers:
        record = json.loads(credential_file.read_text())
        verify_worker_credential(record, workers[0]["id"])
    else:
        require(not credential_file.exists(), "saved worker credential has no matching worker; use a fresh AQ_QUAL_ROOT")
        enrollment = http(server + "/api/enrollments", {}, allow_missing=False)
        record = http(worker_url + "/v1/worker/enroll", dict(code=enrollment["code"], name=name,
                      worker_version=PINS["worker_i0"]["runtime_version"], protocol_version=1), allow_missing=False)
        verify_worker_credential(record, record["node_id"])
        private_bytes(credential_file, json.dumps(record).encode())
    node = dict(node_id=record["node_id"], name=name)
    require(node["node_id"] != "806c44f7-0e2a-4094-b976-687e4e54aedf", "must not reuse the E0 seed node")
    return receipt("node.json", node)


def seed():
    """Graph task, central Git source with a retention checkpoint, catalog rev 1 and binding rev 1."""
    p = layout()
    graph, server, _ = urls()
    node = receipt("node.json")
    require(node is not None, "enroll the fresh qualification node first")
    scratch = PINS["scratch"]
    token = (p["secret"] / "graph_token").read_text().strip()
    workspace = graph + "/v1/workspaces/" + WORKSPACE
    if http(workspace + "/nodes/" + TASK, token=token) is None:
        http(workspace + "/mutations", dict(idempotency_key="e0-qual-task-v1", reason="E0 qualification seed",
             operations=[dict(op="create_node", id=TASK, kind="task", title="E0 qualification scratch task",
                              body="Use only the owner-designated scratch repository.", task_state="active")]),
             token, allow_missing=False)
    branch = "refs/heads/" + scratch["base_branch"]
    registered = git_request("repo.register", dict(binding=dict(identity="e0-qual-scratch", mode="central",
        remote_url=scratch["remote_url"], allow_file_transport=False, allowed_refs=[branch],
        allowed_targets=[branch])), mutation=OP_REGISTER)
    source = receipt("source.json")
    if source is None:
        synced = git_request("repo.sync", dict(refs=[branch]), mutation=OP_SYNC)
        hex_oid = synced.get("refs", {}).get(branch)
        require(isinstance(hex_oid, str) and len(hex_oid) == 40, f"sync gave no {branch} object")
        base = dict(algorithm="sha1", hex=hex_oid)
        created = git_request("workspace.create", dict(base=base), mutation=OP_WORKSPACE)
        checkpoint = git_request("workspace.checkpoint", dict(workspace_id=created["workspace_id"],
            expected_generation=1, expected_head=base, quiescent=True, mode="head"), mutation=OP_CHECKPOINT)
        git_request("workspace.retire", dict(workspace_id=created["workspace_id"],
            expected_generation=checkpoint["workspace_generation"], expected_head=base, quiescent=True),
            mutation=OP_RETIRE)
        retained = git_request("retention.inspect", dict(handle_id=checkpoint["retention_handle"]))
        require(retained["state"] == "active" and any(r["oid"] == hex_oid for r in retained["refs"]),
                "retention handle does not hold the synced base")
        source = receipt("source.json", dict(base=base, workspace_id=created["workspace_id"],
                         retention_handle=checkpoint["retention_handle"], retention=retained,
                         binding_generation=registered.get("binding_generation")))
    artifact = (e0_source() / "scripts/dev-catalog.json").read_text().strip()
    digest = hashlib.sha256(artifact.encode()).hexdigest()
    active = http(server + "/api/profiles/catalogs")
    if active["active_digest"] is None:
        http(server + "/api/profiles/catalogs/activate", dict(operation_id=OP_CATALOG_1, expected_revision=0,
             artifact_json=artifact), allow_missing=False)
    current = http(server + "/api/projects/" + PROJECT)
    if current is None:
        binding = dict(schema_version=1, project_id=PROJECT, name="E0 qualification scratch", enabled=True,
            repository_id=REPOSITORY, repository_binding_generation=1, repository_adapter_reference="aq.git.configured",
            graph_adapter_reference="aq.graph.configured", graph_workspace_id=WORKSPACE, catalog_digest=digest,
            allowed_agent_profiles=["implementer@1"], allowed_intelligence_profiles=["engineering@1"],
            allowed_execution_profiles=["confined-development@1"], allowed_node_ids=[node["node_id"]],
            allowed_worker_groups=[], allowed_environments=["confined-worktree"],
            policy=dict(identity="local-development", revision="1", digest="b" * 64),
            context=[], ceilings=dict(permissions=[dict(operation="task.read", scope_reference="project:example")],
                context_bytes=8192, wall_time_ms=60000, memory_bytes=268435456, cpu_millis=60000, process_limit=8,
                cost_limit_microunits=None, currency=None, require_hard_cost_enforcement=False))
        http(server + "/api/projects/" + PROJECT, dict(operation_id=OP_BINDING_1, expected_revision=0,
             binding=binding), allow_missing=False)
    return receipt("seed.json", dict(project_id=PROJECT, task_id=TASK, workspace_id=WORKSPACE,
                   repository_id=REPOSITORY, node_id=node["node_id"], catalog_rev1_digest=digest,
                   source=source))


MEASURE_VOLUME = PROJECT_NAME + "-measure-codex-home"


def owner_auth_file():
    path = Path(PINS["credential_profile"]["source"]).expanduser()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o600
                and 0 < info.st_size <= 65536, "owner login record must be a private regular file")
        data = source.read()
    try:
        auth = json.loads(data)
        refreshed = auth["last_refresh"]
        require(auth.get("auth_mode") == "chatgpt" and isinstance(auth.get("tokens"), dict), "not a ChatGPT login")
    except (ValueError, KeyError, TypeError):
        raise ValueError("owner login record is not a ChatGPT file login") from None
    import datetime
    age = time.time() - datetime.datetime.fromisoformat(refreshed.replace("Z", "+00:00")[:26] + "+00:00").timestamp()
    require(0 <= age < 7 * 86400, "owner login is older than 7 days; refusing to risk token rotation")
    return path, hashlib.sha256(data).hexdigest()


def docker(*args, timeout=900):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout)
    require(result.returncode == 0, f"docker {args[0]} exited {result.returncode}")
    return result.stdout


def measure():
    """At most three owner-approved provider calls; never re-run once any call started."""
    p = layout()
    image = PINS["images"]["worker_i0"]
    model = PINS["model"]
    raw_path, ledger_path = p["evidence"] / "measure-raw.json", p["evidence"] / "measure-ledger.jsonl"
    require(not raw_path.exists() and not ledger_path.exists(), "measurement budget already spent for this root")
    volumes = docker("volume", "ls", "-q").split()
    require(MEASURE_VOLUME not in volumes, f"stale {MEASURE_VOLUME} volume exists; inspect it before measuring")
    auth, original = owner_auth_file()
    out = Path(tempfile.mkdtemp(prefix="measure-", dir=p["evidence"]))
    out.chmod(0o777)
    common = ["run", "--rm", "--pull=never", "-v", f"{MEASURE_VOLUME}:/c"]
    docker("volume", "create", MEASURE_VOLUME)
    # Clean CODEX_HOME holding only the owner-chatgpt-codex login record, populated immediately before use.
    docker(*common, "--network=none", "--user=0:0", "--cap-drop=ALL", "--cap-add=CHOWN", "--cap-add=DAC_OVERRIDE",
           "--cap-add=FOWNER", "-v", f"{auth}:/owner-auth.json:ro", "--entrypoint=/bin/sh", image, "-c",
           "set -eu; umask 077; chown 10001:10001 /c; chmod 0700 /c; "
           "install -o 10001 -g 10001 -m 0600 /owner-auth.json /c/auth.json; test \"$(ls -A /c)\" = auth.json")
    try:
        docker(*common, "--user=10001:10001", "--read-only", "--tmpfs=/tmp:mode=1777", "--tmpfs=/work:uid=10001,mode=0700",
               "--cap-drop=ALL", "--security-opt=no-new-privileges", "--security-opt=seccomp=unconfined",
               "-e", "CODEX_HOME=/c", "-e", "HOME=/c", "-e", f"AQ_QUAL_MODEL={model['model']}",
               "-e", f"AQ_QUAL_EFFORT={model['native_reasoning']}",
               "-v", f"{QUAL / 'measure-in-image.py'}:/measure.py:ro", "-v", f"{out}:/out",
               "--entrypoint=/usr/bin/python3", image, "-I", "/measure.py", timeout=2400)
    finally:
        if (out / "ledger.jsonl").exists():
            os.replace(out / "ledger.jsonl", ledger_path)
        if (out / "raw.json").exists():
            os.replace(out / "raw.json", raw_path)
        docker(*common, "--network=none", "--user=0:0", "--cap-drop=ALL", "--cap-add=DAC_OVERRIDE",
               "-v", f"{out}:/out", "--entrypoint=/bin/sh", image, "-c", "sha256sum /c/auth.json > /out/after")
        unchanged = (out / "after").read_text().split()[0] == original
        (out / "after").unlink()
        out.rmdir()
        if unchanged:
            docker("volume", "rm", MEASURE_VOLUME)
    record = json.loads(raw_path.read_text()) if raw_path.exists() else None
    summary = dict(calls_started=len(ledger_path.read_text().splitlines()) if ledger_path.exists() else 0,
                   owner_login_unchanged=unchanged,
                   preserved_volume=None if unchanged else MEASURE_VOLUME, raw=str(raw_path) if record else None)
    receipt("measure.json", summary)
    require(record is not None, "measurement produced no record")
    return dict(summary, calls=[{k: c[k] for k in ("call", "exit_code", "usage", "agent_message", "errors", "rollout",
                                               "prompt_utf8_bytes")} for c in record["calls"]])


def down(volumes):
    compose("down", *(["--volumes"] if volumes else []), capture=False)
    return dict(project=PROJECT_NAME, down=True, volumes_removed=volumes)


COMMANDS = dict(prepare=prepare, up=up, enroll=enroll, seed=seed, measure=measure, status=status)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=[*COMMANDS, "configure", "apply", "down", "env"])
    parser.add_argument("--volumes", action="store_true", help="down: also remove this project's own volumes")
    args = parser.parse_args()
    try:
        if args.command == "down":
            result = down(args.volumes)
        elif args.command == "env":
            result = env()
        elif args.command in COMMANDS:
            result = COMMANDS[args.command]()
        else:
            import importlib.util
            spec = importlib.util.spec_from_file_location("e0_qual_definitions", QUAL / "definitions.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            result = getattr(module, args.command)(sys.modules[__name__])
        print(json.dumps(result, sort_keys=True, indent=1))
    except (ValueError, RuntimeError, OSError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"e0-qual-stack {args.command} refused: {error}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
