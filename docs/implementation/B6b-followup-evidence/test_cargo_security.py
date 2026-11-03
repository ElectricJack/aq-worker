"""Run the focused Cargo suite under AQ's foreground test-slot gate."""
import os
from pathlib import Path
import subprocess


def test_cargo_sandbox_security():
    env = dict(os.environ, CARGO_BUILD_JOBS="2", RUST_TEST_THREADS="1", TOKIO_WORKER_THREADS="2")
    log = Path(".aq/tmp/B6b-followup/sandbox-security.log")
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("w") as output:
        result = subprocess.run(
            ["cargo", "test", "--locked", "-p", "aq-worker", "--test", "sandbox_security", "--", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT,
        )
    assert result.returncode == 0, log.read_text()
