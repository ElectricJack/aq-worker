"""Run focused Rust checks under the AQ pytest resource semaphore."""
import os
from pathlib import Path
import subprocess


def test_cargo_check():
    commands = {
        "focused": ["cargo", "test", "--locked", "-p", "aq-session-host", "--lib", "private_inputs::reconciliation"],
        "area": ["cargo", "test", "--locked", "-p", "aq-session-host", "--lib"],
        "clippy": ["cargo", "clippy", "--locked", "-p", "aq-session-host", "--lib", "--tests", "--", "-D", "warnings"],
    }
    check = os.environ["LOGIN_REFRESH_CHECK"]
    command = commands[check]
    result = subprocess.run(
        command,
        cwd=Path(__file__).resolve().parents[3],
        env={**os.environ, "CARGO_BUILD_JOBS": "2", "RUST_TEST_THREADS": "2"},
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=300,
    )
    log = Path(__file__).with_name(f"{check}.log")
    log.write_text(f"COMMAND: {command!r}\n{result.stdout.rstrip()}\n", encoding="utf-8")
    assert result.returncode == 0, result.stdout
