import os, subprocess, shlex
from pathlib import Path
def test_cargo_command():
    cmd=shlex.split(os.environ['B1_CARGO_COMMAND'])
    log=Path(os.environ['B1_CARGO_LOG'])
    with log.open('w') as out:
        result=subprocess.run(cmd,stdout=out,stderr=subprocess.STDOUT,env={**os.environ,'CARGO_BUILD_JOBS':'2'})
    assert result.returncode == 0, log.read_text()[-10000:]
