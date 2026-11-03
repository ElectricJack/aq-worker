# E6a: Local WSL2 worker container — implementation plan

> **For agentic workers:** execute task by task, test-first where code changes.

**Goal:** run the B5b/C2 node controller as a container on WSL2 next to the
dedicated rootless sandbox daemon, autostart it after a Windows logon or WSL
restart, enroll it with node and runner Access tokens, re-point it between
servers, and document the install.

**Architecture:**

- The worker image is built from this repo (`packaging/worker/Dockerfile`), not
  from aq-server's vendored 0.3.1 candidate, which predates B5 and C2.
- It runs on the existing host engine (Docker Desktop through WSL integration)
  as `--user 1000:1000 --init`. Its only daemon mount is the sandbox socket
  directory `$XDG_RUNTIME_DIR/aq-sandbox`. The host Docker socket is never
  mounted.
- A systemd user template unit `aq-worker@<profile>.service` owns the
  container. It requires `aq-sandbox-podman.service` (C2). Linger keeps both
  running without a login session.
- Each server gets a profile:
  `~/.config/agent-q/servers/<profile>/agent-q/worker.toml` is mounted at
  `/config`, and `~/.local/state/agent-q/<profile>` at `/state`. Re-pointing
  means enabling another profile, so identity and journal stay intact.
- On the Windows side, a Task Scheduler logon task runs
  `wsl.exe -d <distro> --exec /bin/sh -c 'exec sleep infinity'`. That boots the
  VM, which starts systemd and the units, and it keeps the VM from idling out.

## Task 1: Secret-safe runner Access token on `enroll` (TDD)

Files: `crates/aq-worker/src/config.rs`, `crates/aq-worker/src/main.rs`.

1. Write the failing tests first.
   - `config::tests::runner_access_save_is_private_json`: `RunnerAccess::save`
     writes `{client_id, client_secret}`. The file is mode 0600, has one link,
     and parses with `deny_unknown_fields`. A second save replaces the file
     atomically. `Debug` redacts the secret.
   - `config::tests::runner_access_path_sits_next_to_worker_toml`.
   - `main::read_secrets_orders_code_cf_runner`: with a scripted reader, the
     order is code → CF id → CF secret → runner id → runner secret. An empty
     runner id or secret is rejected.
   - `main::enroll_never_accepts_runner_token_in_argv`:
     `--runner-access-token=x` and `--runner-access-token x` must not parse as
     a value. The flag is a boolean.
2. Implement:
   - `RunnerAccess`, `runner_access_path`, and `RunnerAccess::save`, reusing
     the private atomic write.
   - `read_secrets_from(reader, cf, runner)`.
   - Enroll writes `runner-access.json` after `worker.toml` and prints only
     its path.
3. Run `aq test -- cargo test -p aq-worker --lib config::` and the
   targeted main tests.

## Task 2: Packaging

- `packaging/worker/Dockerfile`:
  - multi-stage `cargo build --release --locked -p aq-worker`;
  - runtime on bookworm-slim with ca-certificates;
  - `XDG_CONFIG_HOME=/config` and `VOLUME /config /state`;
  - `ENTRYPOINT ["aq-worker"]`.
- `packaging/worker/aq-worker@.service`: a user template unit that runs
  `aq-worker-ctl container-run %i`, with `Restart=always`.
- `packaging/worker/aq-worker-ctl`, a bash script with these subcommands:
  - `install`: copies the units and script, then runs daemon-reload.
  - `build`
  - `enroll <profile> --server URL [...]`: runs the image interactively with
    secrets on stdin or TTY. Secrets never go in argv.
  - `node-config <profile>`: writes a default `node.json`.
  - `use <profile>`: disables the other profiles and enables this one.
  - `status`
  - `container-run <profile>`: the unit's ExecStart.
  - `uninstall`
- `packaging/windows/install-autostart.ps1`: registers or unregisters the
  logon keepalive task and suggests the `.wslconfig` settings.
- Check with `bash -n` and `shellcheck` if present, and `docker build`.

## Task 3: Live verification (in-session, without a host reboot)

1. Build the image.
2. Install and start `aq-sandbox-podman.service`.
3. Enroll a `local` profile against E0 with node and runner tokens.
4. Start `aq-worker@local` and check that `/api/workers` shows it online.
5. `systemctl --user restart aq-worker@local`, then check that it reconciles
   and returns online.
6. Stop podman with `systemctl --user restart aq-sandbox-podman`, then check
   that the worker recovers.
7. Register the scheduled task and confirm it with `schtasks /Query`.

A real Windows restart or `wsl --shutdown` would kill this AQ daemon and every
pool worker. That drill is scripted as `packaging/worker/drill-restart.sh` and
left to the owner.

## Task 4: Docs and evidence

- `docs/operations/install-worker.md` covers prerequisites, install, enroll,
  autostart, re-pointing, uninstall, and what happens when Windows sleeps or
  WSL shuts down.
- `docs/implementation/E6a.md` holds the evidence table, the autostart
  candidates that work, and the remaining drill.

## Resume verification (2026-11-03, claim epoch 3)

- [x] Merge current `origin/main`, preserving the existing implementation and
  dependency merge history.
- [x] Add fast script tests in `packaging/worker/tests/test_worker_ctl.py` for
  stdin enrollment, container mounts/flags, private config and journal
  preservation when switching profiles. First reproduce the enabled but
  unloaded profile case; then enumerate both loaded units and enable links in
  `aq-worker-ctl` for switching and uninstall.
- [x] Exercise `drill-restart.sh` with local fixture snapshots and fake unit
  statuses, including a failing unchanged boot. Do not restart Windows or WSL.
- [x] Run named enrollment/config tests and the packaging tests through
  `aq test`; build the current worker image and record targeted checks.
- [x] Correct the guide's WSL path example and identity location, update
  evidence and explicitly hand off the real reboot drill to the owner.
- [ ] Commit, guarded push, open the required PR, and close the task.
