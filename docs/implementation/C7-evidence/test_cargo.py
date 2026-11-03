"""Resource-managed targeted Cargo checks, with durable output under xdist."""
import json
import os
from pathlib import Path
import subprocess


def test_cargo_command():
    command = json.loads(os.environ['C7_CARGO_COMMAND'])
    assert command[0] == 'cargo' and '--locked' in command
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            env={**os.environ, 'CARGO_BUILD_JOBS': '2', 'RUST_TEST_THREADS': '2'})
    Path(os.environ['C7_CARGO_LOG']).write_text('COMMAND: ' + repr(command) + '\n' + result.stdout)
    assert result.returncode == 0, result.stdout
