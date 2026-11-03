"""Validate the two merged host launch call sites under AQ's test gate."""
import os
from pathlib import Path
import subprocess


def test_cargo_supervisor_session():
    env = dict(os.environ, CARGO_BUILD_JOBS="2", RUST_TEST_THREADS="1", TOKIO_WORKER_THREADS="2")
    log = Path(".aq/tmp/B6b-followup/supervisor-session.log")
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("w") as output:
        result = subprocess.run(
            ["cargo", "test", "--locked", "-p", "aq-session-host", "--test", "supervisor_session"],
            env=env, stdout=output, stderr=subprocess.STDOUT,
        )
    assert result.returncode == 0, log.read_text()
