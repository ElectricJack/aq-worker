# Install a local desktop worker (WSL2)

This guide runs the AQ node worker (B5b controller with the C2 `docker-local`
backend) as a container on a Windows desktop's WSL2 distro. It autostarts after
Windows logon and after a WSL restart, and it reconciles its sandboxes whenever
it starts. Evidence for this setup is in
[`docs/implementation/E6a.md`](../implementation/E6a.md).

```text
Windows logon ─► Task "AQ Docker Desktop at logon"  ─► Docker Desktop (host engine)
              └► Task "AQ WSL worker keepalive"     ─► wsl.exe -d Ubuntu sleep infinity
                                                        (boots the VM and keeps it up)
WSL2 distro: systemd ─► user manager (linger)
   ├─ aq-sandbox-podman.service   rootless Podman API, $XDG_RUNTIME_DIR/aq-sandbox/podman.sock
   └─ aq-worker@<profile>.service docker run --init aq-worker:dev run ...
          /config ◄ ~/.config/agent-q/servers/<profile>    (worker.toml, runner-access.json, node.json)
          /state  ◄ ~/.local/state/agent-q/workers/<profile> (controller journal)
          /run/aq-sandbox ◄ the sandbox socket directory  (its only daemon access)
```

The worker container never gets the host Docker socket. It runs on the host
engine (Docker Desktop) only as an unprivileged container: `--user <uid>:<gid>`,
`--cap-drop ALL`, `no-new-privileges`, a read-only root filesystem and
`--init`. It starts sandboxes only through the dedicated rootless Podman
socket, which is mode 0600 and owned by your WSL user.

## Prerequisites

- Windows 10 22H2 or Windows 11 with WSL 2.x (`wsl --version`). Tested on
  WSL 2.7.10, kernel 6.18 and Ubuntu 24.04.
- systemd enabled in the distro. `/etc/wsl.conf` must contain:

  ```ini
  [boot]
  systemd=true
  ```

  Then run `wsl --shutdown` once from Windows.
- Docker Desktop with WSL integration for the distro (Settings → Resources →
  WSL integration), so `docker version` works inside WSL. A `dockerd` inside
  the distro also works; in that case pass `-NoDockerDesktop` to the Windows
  script.
- The C2 rootless sandbox daemon bundle. Install it with
  `scripts/sandbox-daemon/aq-sandbox-podman.sh install podman-linux-amd64.tar.gz`
  and follow the WSL2 notes in
  [`docs/implementation/C2.md`](../implementation/C2.md) (signature check,
  cgroup v2 delegation).
- Recommended: `sudo apt install uidmap`. Without it the sandbox daemon falls
  back to `AQ_SANDBOX_SINGLE_UID=1`, where every sandbox shares one host UID,
  and `install` warns about this.
- A server URL that the worker can reach **from inside a bridge container**.
  For a server on the same desktop (E0), use `http://host.docker.internal:<port>`.
  `127.0.0.1` would be the container itself.
- An enrollment code from the server, plus a runner Access service token
  (client id and secret) if the server sits behind Cloudflare Access.

## Install

From a checkout of this repository, inside WSL:

```sh
packaging/worker/aq-worker-ctl build      # docker build -> aq-worker:dev (set AQ_WORKER_IMAGE to override)
packaging/worker/aq-worker-ctl install    # ~/.local/bin/aq-worker-ctl, user units, starts aq-sandbox-podman
loginctl enable-linger "$USER"            # units run with no login session; sudo if polkit refuses
```

`install` copies:

- `aq-worker-ctl` and `aq-sandbox-podman.sh` to `~/.local/bin`;
- `aq-worker@.service` and `aq-sandbox-podman.service` to
  `~/.config/systemd/user`.

It then runs `daemon-reload` and `enable --now aq-sandbox-podman.service`. The
worker unit requires the sandbox unit, so restarting or stopping the sandbox
daemon restarts or stops the worker too.

## Enroll

A profile is one server. Pick a short name such as `home` or `local`.

```sh
aq-worker-ctl enroll local --server https://aq.example.com --name my-desktop --runner-access-token
```

You are prompted, without echo, for:

1. the enrollment code;
2. the Cloudflare service token id and secret, only with `--cf-service-token`;
3. the runner Access client id and secret, with `--runner-access-token`.

Without a TTY, the same values are read as lines on stdin, in that order. For
example: `printf '%s\n' "$code" ... | aq-worker-ctl enroll ...`. Avoid typing
secrets into a command line that is saved in shell history.

Secrets are never accepted as arguments. `--runner-access-token` is a flag with
no value. This keeps them out of `ps`, shell history, systemd units and logs,
and enroll prints only file paths.

The result:

| File (host path under `~/.config/agent-q/servers/<profile>/agent-q/`) | Content | Mode |
| --- | --- | --- |
| `worker.toml` | server URL, node id, node credential | 0600 |
| `runner-access.json` | `{client_id, client_secret}` runner Access token | 0600 |
| `node.json` | controller config: `docker_local.socket=/run/aq-sandbox/podman.sock`, capacity, `state_dir=/state/controller` | 0600 |

The directories are 0700. Edit `node.json` to change capacity, the sandbox
ceiling or templates. Delivering the runner token into sandboxes is B6b/B7; for
now the worker only stores it.

Start the worker:

```sh
aq-worker-ctl use local       # enables + starts aq-worker@local, disables every other profile
aq-worker-ctl status
journalctl --user -u aq-worker@local -f
```

When it is healthy, the log shows `node controller starting node_id=… boot_id=…`
followed by `probe succeeded`, and the server lists the node as `online`.

## Autostart

Two layers are needed. Each one is optional only if something else already
provides it.

1. **Inside WSL**: systemd user units plus linger. `aq-sandbox-podman` and
   `aq-worker@<profile>` are `WantedBy=default.target`, and linger starts the
   user manager at VM boot without a login.
   - The worker unit has `Restart=always`, `RestartSec=10` and no start limit,
     so it waits out a late engine or socket.
   - Verified: `loginctl show-user $USER -p Linger` → `yes`.
2. **On Windows**: WSL boots only when something starts it, and it shuts an
   idle VM down. Run this once, from Windows PowerShell:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows\aq-wsl-autostart.ps1 -Distro Ubuntu -StartNow
   ```

   It registers two per-user logon tasks, with no elevation needed:
   - **AQ WSL worker keepalive** runs
     `conhost --headless wsl.exe -d Ubuntu --exec sleep infinity`. That boots the
     VM, which starts systemd, linger and the units. Holding the session open
     keeps WSL from idling the VM out. A second trigger re-fires the task every
     minute with `IgnoreNew`, so a keepalive that ended is back within one or
     two minutes. Restart-on-failure can't do this, because `conhost` exits 0.
   - **AQ Docker Desktop at logon** starts Docker Desktop, the worker
     container's engine. Docker Desktop's own "start when you sign in" setting
     may be off.

   If Task Scheduler refuses, the script falls back to a hidden
   `aq-wsl-keepalive.vbs` in the Startup folder.

   Use `-Status` to check what is installed and `-Uninstall` to remove it.

   To run it from WSL, `cd /mnt/c` first and pass a Windows path:

   ```sh
   cd /mnt/c && powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w packaging/windows/aq-wsl-autostart.ps1)" -Status
   ```

   From a `\\wsl.localhost` working directory, `powershell.exe` fails with
   `UtilAcceptVsock accept4 failed 110`. Even from `/mnt/c` that error can
   happen once; retry.

`.wslconfig` changes are not needed. The keepalive session counts as activity,
so neither `vmIdleTimeout` nor the per-distro idle shutdown applies. Leave
`vmIdleTimeout` at its default. Don't set `vmIdleTimeout=-1` globally to
replace the keepalive: the distro still needs something to boot it at logon.

**Restart drill.** Run this before and after a Windows restart. It records the
evidence and checks: same node id, new boot id, journal advanced, both units
active, server `online`.

```sh
packaging/worker/drill-restart.sh before local http://<server>/api/workers
# restart Windows, log on, wait a minute
packaging/worker/drill-restart.sh after local http://<server>/api/workers
```

## What happens on restart, sleep and WSL shutdown

The controller's identity (`node_id`) and journal live on the `/state` volume,
so they survive container recreation, `wsl --shutdown`, Windows restarts and
image upgrades. Each start is a new `boot_id`.

On each start the controller reconciles the sandboxes it owns before it admits
new work (B5b):

- It lists the sandboxes on the rootless daemon labelled with its own node.
- It re-adopts sandboxes that have a live grant.
- It stops and **preserves** unknown or orphaned sandboxes. A stopped sandbox's
  work volume is kept; nothing is deleted on a timeout.

Sandboxes from a previous boot that were running when WSL stopped come back as
stopped containers. The controller treats them like any other owned inventory.

| Event | Effect |
| --- | --- |
| `systemctl --user restart aq-worker@<p>` or container crash | systemd recreates the container within 10 s; reconcile; online |
| `systemctl --user restart aq-sandbox-podman` | worker restarts with it (Requires=); sandbox containers survive the API restart (`KillMode=process`) |
| `wsl --shutdown`, WSL update, or `wsl --terminate Ubuntu` | everything stops, including running sandboxes. The keepalive task re-fires within one or two minutes and boots the VM; linger starts the units; reconcile |
| Windows sleep or hibernate | the VM is frozen with the host. On resume, wall-clock time jumps. Grants that expired during sleep are not renewed, and the controller's hard-deadline sweep stops those sandboxes and preserves them. Expect the node to show offline while asleep. The suspend drill itself is MS1 |
| Windows restart or sign-out | the logon tasks start Docker Desktop and boot WSL at the next logon; reconcile. Before logon, nothing runs; the tasks are per-user and interactive |
| Docker Desktop quit | the worker container stops; the unit retries every 10 s. Sandboxes are unaffected because they run on Podman, not Docker Desktop |

## Re-point to another server

Each server is its own profile, with its own node identity, credentials and
journal. Re-pointing means switching profiles; nothing is overwritten.

```sh
aq-worker-ctl enroll work --server https://aq.work.example --name my-desktop --runner-access-token
aq-worker-ctl use work      # stops and disables aq-worker@local, starts aq-worker@work
aq-worker-ctl use local     # back again: same node id, same journal
```

Only one profile runs at a time, and they share the rootless sandbox daemon.

- **The old profile's sandboxes stay on the daemon** in whatever state they
  were in. The new profile's controller sees only sandboxes labelled with its
  own node, so it neither adopts nor deletes them.
- Running ones keep running until their local hard deadline. Stop them with
  `aq-sandbox-podman.sh podman stop` if needed; their work volumes are kept.
- When you switch back, the old controller reconciles them like any other
  restart: re-adopt or stop and preserve.
- On the old server, the node simply goes offline. Its grants expire and that
  server reassigns the work.

To retire a profile for good, revoke the node on its server, then delete
`~/.config/agent-q/servers/<p>` and `~/.local/state/agent-q/workers/<p>`. First
check its sandboxes on the daemon with
`aq-sandbox-podman.sh podman ps -a --filter label=aq.node=<node id>`.

Don't change `server_url` in a profile's `worker.toml` to point at a different
server. The node credential belongs to the server that issued it.

## Upgrade

```sh
git pull && packaging/worker/aq-worker-ctl build && systemctl --user restart 'aq-worker@*'
```

The journal format is versioned. An image that can't read it refuses to start
and leaves the journal untouched.

## Uninstall

```sh
aq-worker-ctl uninstall            # disable + remove worker units and the script; keep profiles/journals
aq-worker-ctl uninstall --purge    # also delete ~/.config/agent-q/servers and ~/.local/state/agent-q/workers
systemctl --user disable --now aq-sandbox-podman.service   # optional: the sandbox daemon is left installed
```

On Windows, run `aq-wsl-autostart.ps1 -Uninstall`. Revoke the node on the
server. `--purge` deletes credentials and journals, but not sandbox containers
or volumes on the Podman daemon; remove those first if you want them gone.
