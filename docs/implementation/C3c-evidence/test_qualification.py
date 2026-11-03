"""Explicit C3c physical proofs; source mounts are never committed to images.

Run through AQ with C3C_QUALIFICATION pointing at the recorded non-secret JSON
containing images and repository path/revision pairs. Each test retains its
own source/build directory outside Git for diagnosis.
"""
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

import pytest


def configuration():
    return json.loads(Path(os.environ["C3C_QUALIFICATION"]).read_text())


def run_container(image, command, *, network="none", mounts=(), environment=()):
    argv = ["docker", "--context", "default", "run", "--rm", "--pull=never", "--cpus=2", "--memory=8g",
            "--user", "10002:10002", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--network", network, "--entrypoint", "/bin/sh"]
    for source, target, readonly in mounts:
        argv += ["--mount", f"type=bind,src={source},dst={target}" + (",readonly" if readonly else "")]
    for value in environment:
        argv += ["--env", value]
    argv += [image, "-ec", command]
    print("COMMAND:", json.dumps(argv), flush=True)
    subprocess.run(argv, check=True, timeout=3600)


FINALIZE = ("getent passwd runner && getent passwd agent && command -v tmux && "
            "command -v bwrap && command -v git && test -s /opt/aq/manifest.json")


def test_rust_toolchain_and_finalize_as_agent():
    run_container(configuration()["images"]["rust"], FINALIZE + " && "
                  "test \"$(rustc --version | cut -d' ' -f2)\" = 1.96.1 && "
                  "rustc --version && cargo --version && rustfmt --version && cargo clippy --version && "
                  "test \"$(/opt/aq/node/bin/node --version)\" = v22.16.0")


def test_node_toolchain_and_finalize_as_agent():
    run_container(configuration()["images"]["node"], FINALIZE + " && "
                  "test \"$(node --version)\" = v24.11.0 && test \"$(pnpm --version)\" = 10.34.5 && "
                  "node --version && pnpm --version && "
                  "test \"$(/opt/aq/node/bin/node --version)\" = v22.16.0")


@pytest.mark.parametrize("name", ["aq-server", "aq-worker", "aq-git", "aq-graph", "aq-playbooks", "aq-dashboard"])
def test_committed_repository_builds_inside_template(name):
    cfg = configuration()
    repository = cfg["repositories"][name]
    root = Path(tempfile.mkdtemp(prefix=name + "-", dir=cfg["proof_directory"]))
    source = root / "source"
    source.mkdir()
    archive = root / "source.tar"
    subprocess.run(["git", "-C", repository["path"], "archive", "--format=tar",
                    "--output", str(archive), repository["revision"]], check=True)
    with tarfile.open(archive) as stream:
        stream.extractall(source, filter="data")
    # Only this disposable, private parent directory is made writable to the
    # unprivileged container user. No live checkout, HOME or credential is mounted.
    for path in [root, source, *source.rglob("*")]:
        if not path.is_symlink():
            path.chmod(path.stat().st_mode | 0o077)
    output = root / "build"
    output.mkdir(mode=0o777)
    output.chmod(0o777)
    command = "cargo build --locked" if name != "aq-dashboard" else "pnpm install --frozen-lockfile && pnpm build"
    print("REPOSITORY:", name, "REVISION:", repository["revision"], "ARTIFACTS:", root, flush=True)
    run_container(cfg["images"]["rust" if name != "aq-dashboard" else "node"], command,
                  network="bridge", mounts=[(source, "/workspace", name != "aq-dashboard"), (output, "/build", False)],
                  environment=["CARGO_BUILD_JOBS=2", "RUST_TEST_THREADS=2", "CARGO_HOME=/build/cargo",
                               "CARGO_TARGET_DIR=/build/target", "CI=true", "HOME=/home/agent"])
