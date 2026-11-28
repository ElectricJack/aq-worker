"""ms1-worker-deploy.py's publish, drain, update and rollback decisions against a fake host."""
import importlib.util
import json
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("ms1_worker_deploy", Path(__file__).parents[1] / "ms1-worker-deploy.py")
wd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wd)

OLD, NEW = "a" * 40, "b" * 40
OLD_INSTALLER, NEW_INSTALLER = "1" * 64, "2" * 64


class FakeHost:
    def __init__(self, cfg):
        self.cfg = cfg
        self.manifest = {"installer_sha256": OLD_INSTALLER, "bundle_sha256": "9" * 64, "worker_revision": OLD}
        self.installers = {OLD_INSTALLER: OLD, NEW_INSTALLER: NEW}
        self.time = 1000
        self.now = 0.0
        self.machines = {
            "laptop": self.machine("laptop", ["n1"]),
            "desktop": self.machine("desktop", ["n2", "n3"]),
            "operator": self.machine("operator", ["n4"]),
        }
        self.receivers = {"n1": 0, "n2": 0, "n3": 0}  # node -> age in seconds
        self.jobs = []
        self.sessions_list = []
        self.busy_until = {}          # machine -> clock time its last agent finishes
        self.broken = set()           # installers whose worker never reports in
        self.calls = []
        self.removed = []

    @staticmethod
    def machine(mid, nodes):
        return {"id": mid, "state": "online", "control": {"desired": "running", "generation": "5"},
                "nodes": [{"node": {"id": n}, "health": "online", "worker_version": "0.5.1+" + OLD,
                           "capability_observed_at": "T0000", "agents": [], "slots": {"occupied": 0}} for n in nodes]}

    def pull(self, ref):
        self.calls.append(("pull", ref))

    def image(self, ref):
        return {"Id": "sha256:img", "Config": {"Labels": {wd.REVISION: NEW}}}

    def extract_worker(self, image_id, dest):
        self.calls.append(("extract",))
        return "aq-worker 0.5.1+" + NEW

    def restage(self, revision, base, worker, manifest):
        self.calls.append(("restage", base.name))
        return {"installer_sha256": NEW_INSTALLER, "bundle_sha256": "8" * 64, "worker_revision": revision}

    def swarm(self):
        self.time += 1
        for mid, machine in self.machines.items():
            busy = self.now < self.busy_until.get(mid, -1)
            for n in machine["nodes"]:
                n["slots"]["occupied"] = 1 if busy else 0
        return {"machines": json.loads(json.dumps(list(self.machines.values()))), "server_time": f"T{self.time:04d}"}

    def diagnostics(self):
        return {"server_time_ms": 10 ** 6, "jobs": self.jobs,
                "receivers": [{"node_id": n, "seen_at_ms": 10 ** 6 - age * 1000} for n, age in self.receivers.items()]}

    def sessions(self):
        return self.sessions_list

    def request_update(self, node, operation, installer):
        self.calls.append(("update", node, installer))
        machine = next(m for m in self.machines.values() if any(n["node"]["id"] == node for n in m["nodes"]))
        revision = self.installers[installer]
        ok = installer not in self.broken
        self.jobs.append({"id": operation, "node_id": node, "status": "completed", "result": {
            "ok": True, "details": {"worker": {"version": "aq-worker 0.5.1+" + (revision if ok else "x")}}}})
        if ok:
            self.time += 1
            for n in machine["nodes"]:
                n["worker_version"], n["capability_observed_at"] = "0.5.1+" + revision, f"T{self.time:04d}"

    def admission(self, verb, machine, generation):
        assert str(generation) == self.machines[machine]["control"]["generation"]
        control = self.machines[machine]["control"]
        control["desired"] = "draining" if verb == "drain" else "running"
        control["generation"] = str(int(generation) + 1)
        self.calls.append((verb, machine))

    def read_manifest(self):
        return dict(self.manifest)

    def write_manifest(self, content):
        self.manifest = dict(content)
        self.calls.append(("manifest", content["worker_revision"]))

    def remove_gateway_file(self, name):
        self.removed.append(name)

    def sleep(self, seconds):
        self.now += seconds

    def clock(self):
        return self.now


@pytest.fixture
def cfg(tmp_path):
    path = tmp_path / "cfg.toml"
    path.write_text(f"""
origin = "http://127.0.0.1:28080"
image = "ghcr.io/electricjack/aq-worker:main"
gateway = "{tmp_path / 'gateway'}"
manifest = "{tmp_path / 'gateway/manifest/install-manifest.json'}"
source = "{tmp_path / 'aq-worker'}"
state_dir = "{tmp_path / 'state'}"
drain_timeout_secs = 100
update_timeout_secs = 50
bundles_keep = 2
""")
    config = wd.Config.load(path)
    config.state_dir.mkdir()
    return config


def deployer(cfg):
    host = FakeHost(cfg)
    return wd.Deployer(cfg, host), host


def versions(host, mid):
    return {n["worker_version"] for n in host.machines[mid]["nodes"]}


def test_publishes_then_updates_each_hosted_machine_once(cfg):
    d, host = deployer(cfg)
    d.apply()
    assert ("restage", "9" * 64 + ".tar.gz") in host.calls  # the gateway's current bundle is the base
    updates = [c for c in host.calls if c[0] == "update"]
    assert updates == [("update", "n1", NEW_INSTALLER), ("update", "n2", NEW_INSTALLER)]  # one per machine
    assert versions(host, "laptop") == versions(host, "desktop") == {"0.5.1+" + NEW}
    assert versions(host, "operator") == {"0.5.1+" + OLD}  # no receiver: left alone
    for mid in ("laptop", "desktop"):
        assert host.calls.index(("drain", mid)) < host.calls.index(("start", mid))
        assert host.machines[mid]["control"]["desired"] == "running"
    assert host.manifest["worker_revision"] == NEW


def test_rerun_is_a_no_op(cfg):
    d, host = deployer(cfg)
    d.apply()
    host.calls.clear()
    d.apply()
    assert [c[0] for c in host.calls] == ["pull"]


def test_waits_for_running_agents_before_updating(cfg):
    d, host = deployer(cfg)
    host.busy_until["laptop"] = 30
    d.apply()
    assert host.now >= 30 and versions(host, "laptop") == {"0.5.1+" + NEW}


def test_a_machine_that_stays_busy_is_restored_and_deferred(cfg):
    d, host = deployer(cfg)
    host.busy_until["laptop"] = 10 ** 6
    with pytest.raises(wd.Deferred, match="laptop"):
        d.apply()
    assert not any(c[:2] == ("update", "n1") for c in host.calls)
    assert host.machines["laptop"]["control"]["desired"] == "running"
    assert versions(host, "desktop") == {"0.5.1+" + NEW}


def test_a_hand_started_session_keeps_the_machine_busy(cfg):
    d, host = deployer(cfg)
    host.sessions_list = [{"node_id": "n1", "status": "running"}, {"node_id": "n2", "status": "stopped"}]
    with pytest.raises(wd.Deferred, match="laptop"):
        d.apply()
    assert versions(host, "desktop") == {"0.5.1+" + NEW}


def test_does_not_touch_admission_the_owner_already_stopped(cfg):
    d, host = deployer(cfg)
    host.machines["laptop"]["control"]["desired"] = "draining"
    d.apply()
    assert ("drain", "laptop") not in host.calls and ("start", "laptop") not in host.calls
    assert host.machines["laptop"]["control"]["desired"] == "draining"


def test_failed_update_rolls_back_and_is_not_retried(cfg):
    d, host = deployer(cfg)
    host.broken.add(NEW_INSTALLER)
    with pytest.raises(wd.Failure, match="rolled back"):
        d.apply()
    assert ("update", "n1", OLD_INSTALLER) in host.calls
    assert versions(host, "laptop") == {"0.5.1+" + OLD}
    assert host.machines["laptop"]["control"]["desired"] == "running"
    assert not any(c[:2] == ("update", "n2") for c in host.calls)  # the roll stops at the first failure
    assert host.manifest["worker_revision"] == OLD  # dashboard installs keep the working installer
    host.calls.clear()
    d.apply()
    assert [c[0] for c in host.calls] == ["pull"]


def test_failed_rollback_holds_the_machine_drained(cfg):
    d, host = deployer(cfg)
    host.broken.update({NEW_INSTALLER, OLD_INSTALLER})
    with pytest.raises(wd.Failure, match="left drained"):
        d.apply()
    assert host.machines["laptop"]["control"]["desired"] == "draining"
    state = json.loads((cfg.state_dir / "state.json").read_text())
    assert "laptop" in state["held"]
    d.release("laptop")
    assert "laptop" not in json.loads((cfg.state_dir / "state.json").read_text())["held"]


def test_stale_receiver_or_queued_check_skips_a_machine(cfg):
    d, host = deployer(cfg)
    host.receivers = {"n1": 10 ** 4, "n2": 0}
    host.jobs.append({"id": "other", "node_id": "n2", "status": "queued", "result": None})
    with pytest.raises(wd.Deferred, match="desktop"):
        d.apply()
    assert not any(c[0] == "update" for c in host.calls)


def test_no_hosted_machine_still_publishes_the_manifest(cfg):
    d, host = deployer(cfg)
    host.receivers = {}
    d.apply()
    assert host.manifest["worker_revision"] == NEW and not any(c[0] == "update" for c in host.calls)


def test_refuses_an_image_whose_worker_reports_another_revision(cfg):
    d, host = deployer(cfg)
    host.extract_worker = lambda *_: "aq-worker 0.5.1+" + OLD
    with pytest.raises(wd.Failure, match="reports"):
        d.apply()
    assert not any(c[0] == "restage" for c in host.calls)


def test_pruning_keeps_recent_publications_and_the_manifest():
    published = [{"installer_sha256": f"i{n}", "bundle_sha256": f"b{n}"} for n in range(4)]
    manifest = {"installer_sha256": "i0", "bundle_sha256": "b0"}
    assert wd.bundles_to_prune(published, 2, manifest) == ["i1.sh", "b1.tar.gz"]


def test_a_run_that_died_with_a_machine_drained_restores_it(cfg):
    d, host = deployer(cfg)
    d.apply()
    state = d.load()
    state["drained"] = ["laptop"]
    d.save(state)
    host.machines["laptop"]["control"]["desired"] = "draining"
    d.apply()
    assert host.machines["laptop"]["control"]["desired"] == "running" and d.load()["drained"] == []


def test_publication_requires_the_gateway_to_serve_the_template_image(cfg, monkeypatch):
    """The builder is told which gateway to check, so a bundle whose template image the gateway
    does not serve is never published and no machine is told to update to it."""
    import subprocess
    cfg.gateway.mkdir()
    argv = []

    class Recording(wd.Host):
        def run(self, command, **_kw):
            argv.append([str(a) for a in command])
            return '{"installer_sha256": "i", "bundle_sha256": "b", "worker_revision": "r"}\n'
    monkeypatch.setattr(wd.subprocess, "run", lambda *a, **_kw: subprocess.CompletedProcess(a, 0, b"", b""))
    Recording(cfg).restage(NEW, cfg.gateway / "base.tar.gz", cfg.state_dir / "aq-worker", cfg.state_dir / "m.json")
    builder = argv[-1]
    assert builder[builder.index("--output") + 1] == str(cfg.gateway)
    assert builder[builder.index("--gateway-url") + 1] == "http://127.0.0.1:28443"  # the builder's default listener
    assert builder[builder.index("--nginx") + 1] == "nginx"
    # A refused publication fails the run before any machine is drained or updated.
    d, host = deployer(cfg)
    def refused(*_args):
        raise wd.Failure("prepare-worker-install.py exited 1: The install gateway does not serve 2 of the 4 paths")
    host.restage = refused
    with pytest.raises(wd.Failure, match="does not serve"):
        d.apply()
    assert not any(c[0] in ("drain", "update", "manifest") for c in host.calls)
