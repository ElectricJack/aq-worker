import json
import os
import subprocess

def test_cargo_command():
    command = json.loads(os.environ['C1_CARGO_COMMAND'])
    print('COMMAND:', command, flush=True)
    result = subprocess.run(command, cwd=os.environ['C1_CARGO_CWD'], env={**os.environ, 'CARGO_BUILD_JOBS': '2', 'RUST_TEST_THREADS': '2'})
    assert result.returncode == 0
