#!/usr/bin/env python3
"""Publish the newest main-branch worker to aq-ms1's install gateway and update every hosted machine.

  ms1-worker-deploy.py [--config FILE] apply [--image REF] [--retry-failed]
  ms1-worker-deploy.py [--config FILE] status
  ms1-worker-deploy.py [--config FILE] release MACHINE

`apply` is safe to run from a timer. It follows `image` (the `:main` tag worker-image.yml
moves). For a revision it has not published yet it copies the worker out of the image, checks
that it reports that revision, and republishes the gateway's current bundle with it and with
that revision's installer files (prepare-worker-install.py --base-bundle from a `git archive`
of the revision): the bundle and installer land under `<gateway>/install/<sha256>`.

Then, one machine at a time, every machine with a live hosted receiver that does not run the
revision is drained (`aq worker drain`, only if it was taking work), waited on until no agent
and no hand-started session runs on it, sent `receiver_update_v1` with the new installer, and
checked: the receiver must report the new worker, and every node of the machine must report
the revision in a capability inventory received after the request. Admission is then restored
(`aq worker start`). A machine that never gets idle within drain_timeout_secs is restored and
tried again on the next run. A failed update is rolled back with the previous installer; if
that fails too, the machine stays drained and is held until `release MACHINE`.

The install manifest the server reads (the dashboard's install command) moves to the new
installer once a machine runs it, or at once when no hosted machine exists. Machines without a
receiver (operator-managed profiles) are listed and left alone.

Exit status: 0 done or nothing to do; 1 failed; 2 configuration error; 75 deferred or busy.
"""
import argparse
import contextlib
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_CONFIG = Path.home() / ".config/agent-q/ms1-worker-deploy.toml"
REVISION = "org.opencontainers.image.revision"
WORKER_PATH = "/usr/local/bin/aq-worker"
BUSY_SESSIONS = ("pending", "preparing", "running", "stopping")
DEFERRED = 75


class Failure(Exception):
    pass


class Deferred(Exception):
    pass


@dataclass(frozen=True)
class Config:
    origin: str
    aq: str
    image: str
    gateway: Path
    manifest: Path
    source: Path
    state_dir: Path
    max_workspace_bytes: int | None
    drain_timeout_secs: int
    update_timeout_secs: int
    receiver_fresh_secs: int
    poll_secs: int
    bundles_keep: int
    command_timeout_secs: int

    @staticmethod
    def load(path):
        raw = tomllib.loads(Path(path).read_text())
        cfg = Config(
            origin=raw["origin"], aq=raw.get("aq", "aq"), image=raw["image"],
            gateway=Path(raw["gateway"]), manifest=Path(raw["manifest"]), source=Path(raw["source"]),
            state_dir=Path(raw["state_dir"]), max_workspace_bytes=raw.get("max_workspace_bytes"),
            drain_timeout_secs=raw.get("drain_timeout_secs", 1800),
            update_timeout_secs=raw.get("update_timeout_secs", 900),
            receiver_fresh_secs=raw.get("receiver_fresh_secs", 120),
            poll_secs=raw.get("poll_secs", 10),
            bundles_keep=raw.get("bundles_keep", 5),
            command_timeout_secs=raw.get("command_timeout_secs", 600),
        )
        if cfg.bundles_keep < 2:
            raise ValueError("bundles_keep must be at least 2: the previous installer is the rollback")
        return cfg


def log(message):
    print(f"{datetime.now(timezone.utc):%Y-%m-%dT%H:%M:%SZ} {message}", flush=True)


def bundles_to_prune(published, keep, manifest):
    """Gateway files of publications older than the newest `keep`, unless the manifest names them."""
    kept = {v for entry in published[-keep:] for v in (entry["installer_sha256"], entry["bundle_sha256"])}
    kept |= {manifest.get("installer_sha256"), manifest.get("bundle_sha256")}
    files = []
    for entry in published[:-keep]:
        files += [f"{entry['installer_sha256']}.sh"] if entry["installer_sha256"] not in kept else []
        files += [f"{entry['bundle_sha256']}.tar.gz"] if entry["bundle_sha256"] not in kept else []
    return files


class Host:
    """Every side effect: docker, git, the bundle builder, the aq CLI and the server's HTTP API."""

    def __init__(self, cfg):
        self.cfg = cfg

    def run(self, argv, **kw):
        done = subprocess.run(argv, capture_output=True, text=True, timeout=self.cfg.command_timeout_secs, **kw)
        if done.returncode:
            raise Failure(f"{' '.join(map(str, argv[:3]))} exited {done.returncode}: {done.stderr.strip()[-400:]}")
        return done.stdout

    def pull(self, ref):
        self.run(["docker", "pull", "--quiet", ref])

    def image(self, ref):
        return json.loads(self.run(["docker", "image", "inspect", ref]))[0]

    def extract_worker(self, image_id, dest):
        container = self.run(["docker", "create", image_id]).strip()
        try:
            self.run(["docker", "cp", f"{container}:{WORKER_PATH}", str(dest)])
        finally:
            with contextlib.suppress(Failure):
                self.run(["docker", "rm", container])
        return self.run([str(dest), "--version"]).strip()

    def restage(self, revision, base, worker, manifest):
        """prepare-worker-install.py --base-bundle, as of `revision`, into the gateway."""
        with tempfile.TemporaryDirectory(dir=self.cfg.state_dir) as src:
            self.run(["git", "-C", str(self.cfg.source), "fetch", "--quiet", "origin"])
            archive = subprocess.run(["git", "-C", str(self.cfg.source), "archive", revision, "packaging", "scripts/sandbox-daemon"],
                                     capture_output=True, timeout=self.cfg.command_timeout_secs, check=True).stdout
            subprocess.run(["tar", "-x", "-C", src], input=archive, check=True, timeout=self.cfg.command_timeout_secs)
            argv = [sys.executable, "-I", f"{src}/packaging/service/prepare-worker-install.py", "--base-bundle", str(base),
                    "--worker", str(worker), "--output", str(self.cfg.gateway), "--manifest", str(manifest)]
            if self.cfg.max_workspace_bytes is not None:
                argv += ["--max-workspace-bytes", str(self.cfg.max_workspace_bytes)]
            return json.loads(self.run(argv).splitlines()[-1])

    def http(self, path, body=None):
        request = urllib.request.Request(self.cfg.origin + path, data=None if body is None else json.dumps(body).encode(),
                                         headers={"Content-Type": "application/json", "X-AQ-Request": "1"})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.loads(response.read(), strict=False)
        except urllib.error.HTTPError as error:
            raise Failure(f"{path}: HTTP {error.code} {error.read()[:300].decode(errors='replace')}") from None
        except (urllib.error.URLError, OSError) as error:
            raise Failure(f"{path}: {error}") from None

    def swarm(self):
        reply = self.http("/api/v1/swarm")
        return {**reply["data"], "server_time": reply["server_time"]}

    def diagnostics(self):
        return self.http("/api/worker-diagnostics")

    def sessions(self):
        return self.http("/api/manual-agents")["sessions"]

    def request_update(self, node, operation, installer):
        self.http(f"/api/worker-diagnostics/{node}",
                  {"operation_id": operation, "kind": "receiver_update_v1", "installer_sha256": installer})

    def admission(self, verb, machine, generation):
        self.run([self.cfg.aq, "--local", "--origin", self.cfg.origin, "--json", "worker", verb, "--machine", machine,
                  "--expected-generation", str(generation), "--reason", "ms1-worker-deploy receiver update"])

    def write_manifest(self, content):
        staged = self.cfg.manifest.with_name(self.cfg.manifest.name + ".next")
        staged.write_text(json.dumps(content, indent=2) + "\n")
        staged.chmod(0o644)  # the server container reads it through a bind mount as its own user
        staged.replace(self.cfg.manifest)

    def read_manifest(self):
        return json.loads(self.cfg.manifest.read_text())

    def remove_gateway_file(self, name):
        (self.cfg.gateway / "install" / name).unlink(missing_ok=True)

    def sleep(self, seconds):
        time.sleep(seconds)

    def clock(self):
        return time.monotonic()


def eventually(host, timeout, check, what):
    deadline = host.clock() + timeout
    while True:
        value = check()
        if value:
            return value
        if host.clock() >= deadline:
            raise Failure(f"{what} not reached within {timeout}s")
        host.sleep(host.cfg.poll_secs)


class Deployer:
    def __init__(self, cfg, host):
        self.cfg, self.host = cfg, host
        self.path = cfg.state_dir / "state.json"

    def load(self):
        if self.path.exists():
            return json.loads(self.path.read_text())
        return {"published": [], "failed": {}, "requests": {}, "held": {}, "drained": []}

    def save(self, state):
        staged = self.path.with_suffix(".next")
        staged.write_text(json.dumps(state, indent=2) + "\n")
        staged.replace(self.path)

    # ------------------------------------------------------------ publish

    def publish(self, state, revision, image_id):
        for entry in state["published"]:
            if entry["revision"] == revision:
                return entry
        previous = self.host.read_manifest()
        base = self.cfg.gateway / "install" / f"{previous['bundle_sha256']}.tar.gz"
        with tempfile.TemporaryDirectory(dir=self.cfg.state_dir) as work:
            worker = Path(work) / "aq-worker"
            version = self.host.extract_worker(image_id, worker)
            if not version.endswith("+" + revision):
                raise Failure(f"the image's worker reports {version!r}, not revision {revision}")
            published = self.host.restage(revision, base, worker, Path(work) / "install-manifest.json")
        if published.get("worker_revision") != revision:
            raise Failure(f"the published bundle names worker revision {published.get('worker_revision')}")
        entry = {**published, "revision": revision, "previous": previous}
        state["published"].append(entry)
        self.save(state)
        log(f"published installer {entry['installer_sha256'][:12]} for {revision[:12]} (base bundle {previous['bundle_sha256'][:12]})")
        return entry

    def prune(self, state):
        for name in bundles_to_prune(state["published"], self.cfg.bundles_keep, self.host.read_manifest()):
            self.host.remove_gateway_file(name)
        state["published"] = state["published"][-self.cfg.bundles_keep:]
        self.save(state)

    # ------------------------------------------------------------ machines

    def hosted_machines(self):
        """(machine, receiver node) for every machine with a live receiver; the rest are listed."""
        diagnostics = self.host.diagnostics()
        fresh = {r["node_id"] for r in diagnostics["receivers"]
                 if diagnostics["server_time_ms"] - r["seen_at_ms"] <= self.cfg.receiver_fresh_secs * 1000}
        hosted = []
        for machine in self.host.swarm()["machines"]:
            nodes = [n["node"]["id"] for n in machine["nodes"]]
            receivers = [n for n in nodes if n in fresh]
            if receivers:
                hosted.append((machine, receivers[0]))
            elif machine.get("state") == "online":
                log(f"{machine['id']}: no hosted receiver; left to its operator")
        return hosted

    def machine(self, machine_id):
        swarm = self.host.swarm()
        found = next((m for m in swarm["machines"] if m["id"] == machine_id), None)
        if found is None:
            raise Failure(f"machine {machine_id} is gone from the swarm")
        return found, swarm["server_time"]

    @staticmethod
    def runs(machine, version_suffix):
        return all((n.get("worker_version") or "").endswith(version_suffix) for n in machine["nodes"])

    def idle(self, machine_id):
        machine, _ = self.machine(machine_id)
        nodes = {n["node"]["id"] for n in machine["nodes"]}
        if any(n.get("agents") or n.get("slots", {}).get("occupied") for n in machine["nodes"]):
            return False
        return not any(s.get("node_id") in nodes and s.get("status") in BUSY_SESSIONS for s in self.host.sessions())

    def receiver_update(self, state, machine_id, node, installer, version_suffix):
        key = f"{node}/{installer}"
        operation = state["requests"].setdefault(key, str(uuid.uuid4()))
        self.save(state)
        _, requested_at = self.machine(machine_id)
        self.host.request_update(node, operation, installer)

        def job():
            found = next((j for j in self.host.diagnostics()["jobs"] if j["id"] == operation), None)
            if found is None or found["status"] != "completed":
                return None
            return found["result"] or {}
        result = eventually(self.host, self.cfg.update_timeout_secs, job, f"receiver update {operation}")
        state["requests"].pop(key, None)  # completed: a retry needs a new operation
        self.save(state)
        worker = (result.get("details") or {}).get("worker") or {}
        if not result.get("ok") or "error" in worker or not str(worker.get("version", "")).endswith(version_suffix):
            raise Failure(f"receiver update on {node} reported {json.dumps(result)[:400]}")

        def fresh():
            machine, _ = self.machine(machine_id)
            return all(n.get("health") == "online" and (n.get("worker_version") or "").endswith(version_suffix)
                       and (n.get("capability_observed_at") or "") > requested_at for n in machine["nodes"])
        eventually(self.host, self.cfg.update_timeout_secs, fresh, f"fresh inventory from {machine_id}")

    def update_machine(self, state, machine, node, entry):
        mid = machine["id"]
        before = next(n["worker_version"] for n in machine["nodes"] if n["node"]["id"] == node)
        drained = machine["control"]["desired"] == "running"
        if drained:
            # Recorded first, so a run that dies here is followed by one that restores admission.
            state["drained"] = sorted({*state.get("drained", []), mid})
            self.save(state)
            self.host.admission("drain", mid, machine["control"]["generation"])
            log(f"{mid}: drained")
        held = False
        try:
            try:
                eventually(self.host, self.cfg.drain_timeout_secs, lambda: self.idle(mid), f"{mid} idle")
            except Failure:
                raise Deferred(f"{mid}: still running work after {self.cfg.drain_timeout_secs}s; trying again next run") from None
            try:
                self.receiver_update(state, mid, node, entry["installer_sha256"], "+" + entry["revision"])
                log(f"{mid}: runs {entry['revision'][:12]} with a fresh inventory")
            except Failure as error:
                log(f"{mid}: update failed ({error}); rolling back to {before}")
                try:
                    self.receiver_update(state, mid, node, entry["previous"]["installer_sha256"], before)
                except Failure as rollback:
                    held = True
                    state["held"][mid] = f"update to {entry['revision']} failed ({error}); rollback failed ({rollback})"
                    self.save(state)
                    raise Failure(f"{mid}: {state['held'][mid]}; left drained") from None
                raise Failure(f"{mid}: update failed and was rolled back: {error}") from None
        finally:
            if drained and not held:
                self.restore_admission(state, mid)

    def restore_admission(self, state, mid):
        try:
            current, _ = self.machine(mid)
            if current["control"]["desired"] != "running":
                self.host.admission("start", mid, current["control"]["generation"])
            log(f"{mid}: admission restored")
        except Failure as error:
            log(f"{mid}: admission not restored yet ({error}); the next run tries again")
            return
        state["drained"] = [m for m in state.get("drained", []) if m != mid]
        self.save(state)

    # -------------------------------------------------------------- apply

    def apply(self, ref=None, retry_failed=False):
        state = self.load()
        ref = ref or self.cfg.image
        self.host.pull(ref)
        info = self.host.image(ref)
        revision = (info["Config"]["Labels"] or {}).get(REVISION, "")
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise Failure(f"{ref} has no source revision label")
        for mid in state.get("drained", []):
            if mid not in state["held"]:
                self.restore_admission(state, mid)
        if revision in state["failed"] and not retry_failed:
            log(f"skipping {revision[:12]}: it failed here before ({state['failed'][revision]})")
            return
        entry = self.publish(state, revision, info["Id"])
        hosted = self.hosted_machines()
        deferred = []
        for machine, node in hosted:
            if self.runs(machine, "+" + revision):
                continue
            ours = set(state["requests"].values())
            blocking = [j["id"] for j in self.host.diagnostics()["jobs"]
                        if j["node_id"] == node and j["status"] in ("queued", "running") and j["id"] not in ours]
            if blocking:
                log(f"{machine['id']}: worker check {blocking[0]} is still queued on {node}; trying again next run")
                deferred.append(machine["id"])
                continue
            if machine["id"] in state["held"]:
                log(f"{machine['id']}: held ({state['held'][machine['id']]}); run `release {machine['id']}`")
                continue
            try:
                self.update_machine(state, machine, node, entry)
            except Deferred as error:
                log(str(error))
                deferred.append(machine["id"])
            except Failure as error:
                state["failed"][revision] = str(error)[:400]
                self.save(state)
                raise
        # The dashboard's install command names an installer only once a machine runs it.
        if not hosted or any(self.runs(self.machine(m["id"])[0], "+" + revision) for m, _ in hosted):
            manifest = {k: entry[k] for k in ("installer_sha256", "bundle_sha256", "worker_revision")}
            if self.host.read_manifest() != manifest:
                self.host.write_manifest(manifest)
                log(f"install manifest now names {revision[:12]}")
        self.prune(state)
        if deferred:
            raise Deferred(f"deferred: {', '.join(deferred)}")

    def status(self):
        state = self.load()
        machines = {m["id"]: sorted({n.get("worker_version") for n in m["nodes"]}) for m in self.host.swarm()["machines"]}
        return {**state, "manifest": self.host.read_manifest(), "machines": machines}

    def release(self, machine_id):
        state = self.load()
        state["held"].pop(machine_id, None)
        self.save(state)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    sub = parser.add_subparsers(dest="command", required=True)
    apply = sub.add_parser("apply")
    apply.add_argument("--image", help="deploy this reference instead of the configured image")
    apply.add_argument("--retry-failed", action="store_true")
    sub.add_parser("status")
    release = sub.add_parser("release")
    release.add_argument("machine")
    args = parser.parse_args(argv)
    os.umask(0o077)  # bundle contents stay private to this user, like the operator's hand builds
    try:
        cfg = Config.load(args.config)
        cfg.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    except (OSError, KeyError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"ms1-worker-deploy: configuration error: {error}", file=sys.stderr)
        return 2
    deployer = Deployer(cfg, Host(cfg))
    with open(cfg.state_dir / "lock", "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log("another ms1-worker-deploy holds the lock")
            return DEFERRED
        try:
            if args.command == "apply":
                deployer.apply(args.image, args.retry_failed)
            elif args.command == "status":
                print(json.dumps(deployer.status(), indent=2))
            else:
                deployer.release(args.machine)
        except Deferred as error:
            log(str(error))
            return DEFERRED
        except (Failure, subprocess.SubprocessError) as error:
            log(f"failed: {error}")
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
