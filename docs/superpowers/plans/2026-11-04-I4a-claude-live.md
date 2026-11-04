# I4a (re-scoped): Claude Code CLI live qualification in docker-local

> **For agentic workers:** execute inline with superpowers:executing-plans.
> Task `azure-horizon-22.2`. Re-scoped by the owner on 2026-11-04 from Codex
> to the Claude Code CLI with the owner's subscription login. Codex moves to a
> later follow-up.

**Goal:** prove that the Claude Code CLI, pinned in an image derived from the
C3c rust template, completes an unattended edit, check and publish cycle in a
real docker-local sandbox driven by the B5a node controller. Auth comes only
from `CLAUDE_CODE_OAUTH_TOKEN`, leased as private material, and the sandbox is
wiped on teardown.

**Architecture:** one new ignored integration test file,
`crates/aq-worker/tests/claude_live.rs`.

- `Controller<Shared, Lease>` drives the real `DockerLocalBackend`.
- `Shared` wraps the backend in `Arc<tokio::Mutex<_>>` so the test can call
  `runner_exit` and `read_file` on the sandbox the controller owns.
- `Lease` is a test `PermitProvider`. It issues a preparation permit bound to
  the attempt, and its private payload is the token read from the owner's
  0600 file.
- The runner is a fixed bash script in `runner_command`. It carries no secret.
  It reads the material file into the env variable, deletes the file, and
  then does the following:
  1. builds a tiny crate with a failing test and confirms the RED;
  2. runs `claude -p --permission-mode acceptEdits --allowedTools Read,Edit`;
  3. runs `cargo test` (the check);
  4. commits and pushes to a bare repo in the work volume (the publish);
  5. scans `$HOME`, `/aq` and `/tmp` for the token, using a pattern from a
     file descriptor, never from argv;
  6. writes `out/result.json`.

**Not in scope:** C6a and C6b are not merged, so this task does not cover the
runner confinement mode or the credential lease service. On this host the
sandbox Podman runs in single-UID mode, so no non-root UID can start under
the backend's fixed policy. The runner therefore runs as in-container uid 0:
host uid 1000, CapEff 0, and no-new-privileges. The evidence doc records both
as limits and gives the owner fix.

## Task 1: derived image (done before the plan, recorded in evidence)

- `docs/implementation/I4a-evidence/Containerfile.claude-rust`, built `FROM`
  the rust-1 digest, installs `@anthropic-ai/claude-code@2.1.318` (the native
  executable) and links it at `/opt/aq/bin/claude`.
- Push it to the local unadmitted registry, pull it by digest into the
  sandbox Podman, and record the manifest digest.

## Task 2: failing live test (RED)

- [ ] Write `claude_live.rs` containing:
  - `live_claude_subscription_edit_check_publish_cycle`, which asserts the
    runner exit, `result.json` fields, publish oid == head oid, the material
    wipe, zero token hits, and no `.credentials.json`;
  - an unchanged host login fingerprint;
  - after teardown, stop proof, destroy proof, and an empty `list_owned`.
- [ ] Run it with the runner script stubbed to `exit 3`. Expect FAIL on the
  runner exit assertion.

## Task 3: runner script (GREEN)

- [ ] Fill in the script. Run the live test and expect PASS. Save the
  read-back outputs to `I4a-evidence/` through `AQ_CLAUDE_LIVE_EVIDENCE_DIR`.
- [ ] Run it again for a second consecutive pass.

## Task 4: checks

- [ ] `cargo test --locked -p aq-worker --test claude_live`, which runs the
  pure tests and leaves the live test ignored.
- [ ] The live command above with `-- --ignored`, through the C1 cargo wrapper
  with `CARGO_BUILD_JOBS=2`.
- [ ] `cargo clippy -p aq-worker --tests` limited to the new file, and
  `rustfmt --edition 2024 --check` on the new file.

## Task 5: evidence

- [ ] `docs/implementation/I4a.md`: decisions, image digest, CLI version,
  commands, H2 rows, the acceptance map, limits, and the follow-ups that go in
  the close summary (no children).
