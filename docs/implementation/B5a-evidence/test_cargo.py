"""Run exact Cargo commands inside AQ's existing pytest resource gate."""
import os
import json
import shlex
import subprocess
from pathlib import Path

def test_cargo_command():
    command = shlex.split(os.environ["B5A_COMMAND"])
    log = Path(os.environ["B5A_LOG"])
    with log.open("w") as out:
        result = subprocess.run(command, stdout=out, stderr=subprocess.STDOUT,
                                env={**os.environ, "CARGO_BUILD_JOBS": "2"})
    if record := os.environ.get("B5A_RESULT"):
        Path(record).write_text(json.dumps({"command": command,
            "exit_code": result.returncode, "log": str(log)}, indent=2) + "\n")
    assert result.returncode == 0, log.read_text()[-12000:]
