# E6a: Local WSL2 worker container

Task `smart-falcon-16.8`, claim epoch 1, branch `aq/smart-falcon-16.8`.

- Source base: `7979083`.
- Plan: `docs/superpowers/plans/2026-11-02-E6a-local-wsl2-worker.md`, committed
  before source changes as `df9df92`.
- E6a is not marked for adversarial review. It uses direct integration.
- Operator guide: [`docs/operations/install-worker.md`](../operations/install-worker.md).

## What was built

| Piece | Path | Notes |
| --- | --- | --- |
| Worker image | `packaging/worker/Dockerfile` (+ `.dockerignore`) | Two stages, both base images pinned by digest. Build: `cargo build --locked --release -p aq-worker`. Runtime: bookworm-slim with ca-certificates. `XDG_CONFIG_HOME=/config`, `HOME=/state`, `VOLUME /config /state`, `ENTRYPOINT aq-worker`. Built from this repo, not aq-server's vendored 0.3.1 candidate, which predates B5b and C2. |
| Container unit | `packaging/worker/aq-worker@.service` | Systemd user template, one instance per profile. `Requires=`/`After=aq-sandbox-podman.service` (C2). `Restart=always`, `RestartSec=10`, no start limit. `ExecStop=docker stop --timeout 30`. |
| Control script | `packaging/worker/aq-worker-ctl` | Subcommands: `build`, `install`, `enroll`, `node-config`, `use`, `status`, `container-run` and `uninstall [--purge]`. The container runs with `--init --user uid:gid --cap-drop ALL --security-opt no-new-privileges --read-only` and a `/tmp` tmpfs. Mounts: `/config`, `/state`, and the sandbox socket directory at `/run/aq-sandbox`. **No host Docker socket.** |
| Windows autostart | `packaging/windows/aq-wsl-autostart.ps1` | Per-user logon tasks. "AQ WSL worker keepalive" runs `conhost --headless wsl.exe -d <distro> --exec sleep infinity`, with a logon trigger plus a one-minute re-fire trigger and `IgnoreNew`. "AQ Docker Desktop at logon" starts Docker Desktop. Fallback: Startup-folder launcher. Also `-Status` and `-Uninstall`. |
| Restart drill | `packaging/worker/drill-restart.sh` | `before`/`after` snapshots and pass/fail checks for the Windows restart. |
| Runner Access token | `crates/aq-worker/src/config.rs`, `crates/aq-worker/src/main.rs` | See below. |

### Runner Access token (secret-safe)

`aq-worker enroll --runner-access-token` is a boolean flag. It never takes a
value.

1. After the enrollment code and the optional CF service token,
   `read_secrets_from` reads `Runner Access client id` and
   `Runner Access client secret`. On a TTY they come from `rpassword` without
   echo; otherwise as stdin lines. Both are trimmed, and an empty value is
   rejected.
2. `RunnerAccess::save` writes `runner-access.json` next to `worker.toml` with
   `{client_id, client_secret}` and `deny_unknown_fields`.
   - It goes through the shared `write_private` helper: 0700 directory, a
     `create_new` 0600 temp file, fsync, then rename.
   - `WorkerConfig::save` now uses the same helper.
3. `Debug` redacts the secret. Enroll prints only the path.

So the token never appears in argv, shell history, unit files or logs.
Delivering it into sandboxes is B6b/B7 and is out of scope.

## Autostart: which candidates work

Measured on WSL 2.7.10, kernel 6.18.33.2, Ubuntu 24.04, Docker Desktop 29.3.1
and Podman 5.8.8 (static bundle). Log: `E6a-evidence/autostart.log`.

| Candidate | Result |
| --- | --- |
| systemd user units + `loginctl enable-linger` | **Works.** Linger was enabled without sudo through polkit. Units are `WantedBy=default.target`, and the worker restarts itself after a crash, a kill or a sandbox-daemon restart (see the reconcile table). |
| Task Scheduler logon task running `wsl.exe … sleep infinity` | **Works** for boot-at-logon and VM keepalive. Registered without elevation; `State=Running`. |
| Task Scheduler restart-on-failure | **Does not work.** `conhost --headless` exits 0 whatever `wsl.exe` returns (it returned 15 for a killed session). After a kill the task sat in `Ready` with `LastTaskResult=0`. |
| Repetition on the logon trigger | Not effective until the next logon (`NextRunTime` empty). |
| Extra time trigger, every minute, indefinite, `IgnoreNew` | **Works.** A killed keepalive came back by the next fires (23:16:07, then 92 s after a second kill), with exactly one instance across later fires. |
| Docker Desktop's own autostart | **Off on this machine** (`settings-store.json AutoStart=False`). A `Run` key exists, but whether it fires is unproven. Hence the explicit "AQ Docker Desktop at logon" task, which is registered and `Ready`. |
| `.wslconfig` `vmIdleTimeout` | Not needed: the keepalive session counts as activity. It can't replace boot-at-logon. Left at the default. |
| Startup-folder `.vbs` launcher | Implemented as the fallback; not exercised because registration succeeded. |

## Live verification (no host reboot)

Enrolled the `local` profile against E0 (`http://host.docker.internal:32839`)
with `--name e6a-wsl-desktop --runner-access-token`.

- The enrollment code was minted through E0's API and piped through stdin; it
  was never printed.
- E0 has no Access, so the runner id and secret are placeholders.

The result was node `6f00545b-ff6f-4024-a377-3a0d74a8b63f`, and
`GET /api/workers` shows it `online`, worker_version 0.5.1.

| Check | Evidence (`E6a-evidence/`) | Result |
| --- | --- | --- |
| Container shape | `live-state.log` | `Init=true`, user 1000:1000, read-only root, `CapDrop=[ALL]`, `no-new-privileges`. Exactly three mounts: config, state and the sandbox socket directory. `docker.sock mounted: 0`. PID 1 is `docker-init`. |
| Credential files | `live-state.log` | `worker.toml`, `runner-access.json` and `node.json` are 0600 with one link; profile directories and `servers/` are 0700. Socket `podman.sock` is 0600, owned by uid 1000. |
| Unit restart | `boot-history.log` | `systemctl --user restart aq-worker@local` gives the same node_id, a new boot_id and `probe succeeded`. |
| Container killed (137) | `boot-history.log` | systemd restarted it after 10 s with a new boot_id. |
| Sandbox daemon restart | `boot-history.log` | The worker stopped with it (Requires=). The first restart raced the socket (exit 1), and it was back 10 s later with a new boot_id and online. |
| Reconcile real inventory | `orphan-reconcile.log` | A planted owned sandbox from an earlier boot was stopped and **preserved** (container exited, volume kept, journaled `UnknownSandbox`). After a restart, the new boot still held it as stopped and preserved. The drill container and volume were then removed. |
| Restart drill script self-test | `drill-restart-selftest.log` | `before` → unit restart → `after`: all six checks `PASS`, `DRILL PASS`. Stands in for the reboot step only. |
| Re-point | `repoint.log` | Enrolled a second profile, `repoint` (node `14fb58d6…`, online). `use repoint` stopped `local`, leaving exactly one worker container. `use local` brought back the same node_id with its journal advanced from rev 22 to 26 and the orphan record kept. Both journals are retained. |
| Journal identity | `boot-history.log` | Six controller boots, all with `node_id=6f00545b-…`, each with a distinct boot_id. |

Tests and checks were run with Rust 1.96.1, `CARGO_BUILD_JOBS=2`. `aq test` only
wraps pytest, so Cargo was run directly.

| Command | Result | Log |
| --- | --- | --- |
| `cargo test --locked -p aq-worker --lib config::` | 2 passed (`runner_access_save_is_private_json`, `runner_access_path_sits_next_to_worker_toml`) | `config-tests.log` |
| `cargo test --locked -p aq-worker --bin aq-worker -- read_secrets enroll_never run_requires` | 3 passed (`read_secrets_orders_code_cf_runner`, `enroll_never_accepts_runner_token_in_argv`, existing `run_requires_explicit_controller_configuration`) | `enroll-tests.log` |
| `cargo fmt -p aq-worker --check` | exit 0 | `fmt.log` |
| `cargo clippy --locked -p aq-worker --lib --bin aq-worker -- -D warnings` | exit 0 | `clippy.log` |
| `bash -n` on both shell scripts | exit 0 | `shell-syntax.log` |
| `docker build -f packaging/worker/Dockerfile` | exit 0, `aq-worker:e6a-dev` (106 MB), tagged `aq-worker:dev` | build output kept locally, not committed |

The tests were written first. Before the implementation, the new tests failed
to compile because `RunnerAccess`, `runner_access_path` and `read_secrets_from`
did not exist. This was not a behavioural red run. `shellcheck` is not
installed, so it was not run. No full workspace suite was run, following the
velocity rule.

## Acceptance status

| Acceptance item | Status |
| --- | --- |
| Worker container: B5b + C2 docker-local, sandbox socket, `/config` + `/state`, `--init`, no host Docker socket | Done (live) |
| Autostart after Windows logon and WSL restart; VM kept alive; candidates recorded | Mechanisms installed and verified piecewise (linger, units, both logon tasks, keepalive re-fire). Logon itself not yet observed. |
| Enroll with node + runner tokens; runner token never in argv, history or logs; 0600 | Done (tests + live) |
| Re-point between servers keeping identity and journal; sandbox behaviour documented | Done (live + guide) |
| `install-worker.md` (prerequisites, install, enroll, autostart, re-point, uninstall, sleep/shutdown) | Done |
| **Fresh install, enroll, Windows restart → worker back online and reconciled (recorded drill)** | **Not done.** A real Windows restart or `wsl --shutdown` kills the AQ daemon and every pool worker, including the session doing this task, so it can't be run from inside it. It must be run by the owner: `drill-restart.sh before local http://127.0.0.1:32838/api/workers`, restart Windows, log on, then `drill-restart.sh after …`. Append the log, `~/.local/state/agent-q/drill-restart.log`, here. |

## Known limits and notes

- `uidmap` is not installed, so the sandbox daemon runs with
  `AQ_SANDBOX_SINGLE_UID=1`, as in C2. `install` warns about this.
- The planted orphan's journal record remains after the drill cleanup. Clearing
  it is B5b's operator-release path; harmless.
- The `repoint` demo profile (node `14fb58d6…`) is left enrolled and disabled
  on E0. Remove it with `rm -rf ~/.config/agent-q/servers/repoint
  ~/.local/state/agent-q/workers/repoint`.
- WSL interop (`powershell.exe`, `schtasks.exe`) sometimes fails with
  `UtilAcceptVsock accept4 failed 110`, always from a `\\wsl.localhost`
  working directory and occasionally from `/mnt/c`. Retrying works.
- Bridge containers reach a server on the same desktop through
  `host.docker.internal`. Loopback is the container itself.
- The plan named `~/.local/state/agent-q/<profile>` as the state root.
  Implementation moved it to `~/.local/state/agent-q/workers/<profile>`, because
  `~/.local/state/agent-q` holds unrelated state that `uninstall --purge` must
  not delete.
- The plan also named `install-autostart.ps1`; the file is
  `aq-wsl-autostart.ps1`.
