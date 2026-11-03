"""Run one Cargo check under AQ admission and retain its complete output."""
import json
import os
from pathlib import Path
import subprocess


def test_cargo_command():
    command = json.loads(os.environ['BOLD_HARBOR_COMMAND'])
    output = Path(os.environ['BOLD_HARBOR_LOG'])
    with output.open('w') as log:
        log.write('COMMAND: ' + json.dumps(command) + '\n')
        log.write('CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2\n')
        log.flush()
        result = subprocess.run(
            command,
            cwd=os.environ['BOLD_HARBOR_CWD'],
            env={**os.environ, 'CARGO_BUILD_JOBS': '2', 'RUST_TEST_THREADS': '2'},
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    assert result.returncode == 0, output.read_text()
