# Codex Docker seccomp qualification

Task: `sharp-dune-88`; observed 2026-11-02 (America/Los_Angeles).

[qualification.json](qualification.json) retains 12 passing expectations from:

```sh
python3 tools/qualification/probe-codex-docker.py --output .aq/tmp/codex-docker/final-qualification
```

The emitted JSON was copied here without changing its contents. Default-profile
and removed-allowance failures are expected negative checks, not successful
sandbox launches. `passed: true` means every expected exit and diagnostic was
observed. The metadata check verifies executable versions/hashes, UID/GID,
capabilities, NNP, seccomp and read-only root in the real image. The workspace
canary additionally checks these process restrictions inside Codex's sandbox.

Every Docker request is recorded as an argument array. Temporary removal-profile
paths no longer exist; the probe deterministically regenerates them from the
delivered profile. Their SHA256 values are recorded with each request. The
qualification script and delivered profile are committed together with this
evidence. There were no provider calls, credentials or host bind mounts.

The opt-in support bounds, profile source/license, exact syscall delta and
unqualified scenarios are in the
[operations guide](../../../operations/codex-docker-security.md).

Offline verification:

- `aq test tools/qualification/tests/test_codex_docker_seccomp.py -q`: 2 passed,
  using AQ's configured fallback Python and six-worker cap.
- `aq test tools/qualification/tests -q`: 5 passed with the same resource controls.
- `python3 -m py_compile tools/qualification/probe-codex-docker.py tools/qualification/tests/test_codex_docker_seccomp.py`: passed.
- `git diff --check`: passed.
- Relative Markdown links in this evidence note and the operations guide resolve.

No Rust/runtime defaults changed; a workspace-wide Rust suite was not needed
for these documentation, profile and explicit Python probe changes.
