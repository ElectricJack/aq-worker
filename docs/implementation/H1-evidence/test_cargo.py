"""Run the exact H1 Cargo command under AQ's pytest resource gate."""
import os
import shlex
import subprocess
from pathlib import Path

def test_cargo_command():
    command = shlex.split(os.environ['H1_COMMAND'])
    log = Path(os.environ['H1_LOG'])
    with log.open('w') as stream:
        result = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT,
            env={**os.environ, 'CARGO_BUILD_JOBS': '2', 'RUST_TEST_THREADS': '2'})
    assert result.returncode == 0, log.read_text()[-16000:]
