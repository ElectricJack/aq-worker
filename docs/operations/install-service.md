# Install the aq-worker service (Linux and WSL2)

This installs `aq-worker` as an always-on systemd user service with one or more
slots, enrolled with a single-use code. It is separate from the per-profile
container worker in [install-worker.md](install-worker.md). macOS is not
supported yet; the script refuses it.

## Prerequisites

- Linux, or WSL2 with systemd. For WSL2 follow "Prerequisites" in
  [install-worker.md](install-worker.md) (`/etc/wsl.conf` with `[boot]
  systemd=true`, then `wsl --shutdown` once).
- Linger, so the user manager runs without a login: `sudo loginctl enable-linger
  $USER`. The script prints this command when linger is off and continues.
- WSL2 autostart from Windows (`aq-wsl-autostart.ps1`): see "Autostart" in
  [install-worker.md](install-worker.md).
- WSL2 host load: see "Windows host load (WSL2)" in
  [install-worker.md](install-worker.md). The script prints the probe command
  when the host-load file is missing and never runs it.
- The rootless sandbox daemon: either its socket already exists at
  `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock`, or pass `--sandbox-bundle
  podman-linux-amd64.tar.gz` and the script installs and starts it. Without
  either it exits with code 4.
- A single-use enrollment code, created from the Swarm page ("Add machine").

## The bundle directory

`--bundle DIR` holds:

- the five binaries `aq-worker`, `aq-session-host`, `aq-agent`, `aq-git` and
  `aq-sandbox-driver`;
- `native.json` and `repositories.json`.

Copy `native.json` and `repositories.json` from an enrolled machine of the same
build. `native.json` pins exact image and tool digests, so a file from another
build will not match the binaries. `--native-template` and `--repositories`
override the two defaults.

## Install

```sh
packaging/service/aq-install.sh --server https://aq.example --bundle ~/aq-bundle \
  --profile local --slots 4 --machine-key my-box
```

Options: `--profile` (default `local`; lower-case letters, digits and `-`, at
most 32 characters), `--slots` (default 1, at most 8), `--name`,
`--machine-key`, `--cf-service-token`, `--native-template`, `--repositories`,
`--measurement-token-file` (an absolute path) with `--measurement-model`,
`--sandbox-bundle` and `--reconfigure`.

The script refuses to replace a `~/.local/bin/aq-worker` that is not its own
link, and warns when the container worker `aq-worker@local` is running too.

The script, in order:

1. checks the platform, systemd, linger and the machine key;
2. makes sure the sandbox daemon is running;
3. installs the five binaries into `~/.local/share/agent-q/runtime/` and links
   `~/.local/bin/aq-worker`;
4. runs `aq-worker service enroll`, which prompts for the code. A profile that
   is already enrolled is skipped (pass `--reconfigure` to enroll it again);
5. runs `aq-worker service install` (writes the user unit `aq-node@PROFILE`
   and starts it, or restarts it when it is already running);
6. runs `aq-worker service measure` when a measurement token file was given.
   It holds the profile's measurement lock, so the service does not start a
   second measurement meanwhile;
7. prints the Windows probe command on WSL2 if needed;
8. prints `aq-worker service status`.

The enrollment code is typed at the prompt, or piped as the first stdin line.
With `--cf-service-token` the next two prompts (or lines) are the node's Access
client id and secret. Nothing secret is ever put in an argument, the
environment, a log or a unit file. The code works once.

`enroll` checks every argument (the server origin, the repositories file, the
token file path) before it reads the code, so a typo never uses it up. The
server must be `https`; plain `http` is accepted only to `127.0.0.1` or `::1`.
If the machine key claim fails after the code was redeemed (for example the key
is in use), run the same command again: it resumes at the claim and needs no
new code. If Cloudflare Access refused the stored token at the claim, resume
with `--reconfigure --cf-service-token` and the new Access client id and secret.
The slot credentials are written only once the claim succeeds, all of them
together. If a run stopped after the claim but before it saved the slots, the
rerun says so: revoke the machine and enroll again with a new code.

### The machine key

The machine key is the one name for the machine: the Swarm page shows it, and
start, stop, drain and revoke act on it. It is `--machine-key`, or else the
lower-cased host name. It must match `[a-z0-9][a-z0-9-]{0,62}`. Slot 1 claims it
on the server; the other slots are minted under it. The machine's label is the
key too, unless `--name` gives another.

### Where the service keeps its files

- Configuration: `~/.config/agent-q/service/PROFILE/` (`service.toml`,
  `machine.json`, `native.json`, one `slots/N/worker.toml` per slot).
- Durable state: `state_root` in `service.toml`, by default
  `~/.local/state/agent-q/service/PROFILE/`: each slot's journal, Git state,
  materials and archives.
- Runtime files: every Unix socket (aq-git's, tmux's, the session host's) and
  the session host's per-launch directories live under a short runtime root,
  `$XDG_RUNTIME_DIR/aq/PROFILE/` (for example `/run/user/1000/aq/local/`), or
  `/tmp/aq-UID/PROFILE/` when `XDG_RUNTIME_DIR` is unset. Each slot has
  `N/git.sock` and the host root `N/host/` there. The service creates the root
  0700 and refuses one that is a symlink, belongs to another user or is open to
  others. These files do not survive a reboot, and need not.

A Unix socket path holds at most 103 bytes here, so `service.toml` is refused
at load (and `enroll` refuses before it reads the code) when a slot's socket
path would be longer; the error names the path and its length. With the
default runtime root this only happens for a long profile name (more than about
20 characters under `/run/user/1000`). Use a shorter profile, or set
`runtime_root = "/absolute/short/dir"` in `service.toml`: its parent must be
owned by you and writable only by you.

### Allowing the machine

There is no allow-list step. An enrolled machine is allowed automatically on
every credential and project that sets `allow_enrolled_nodes = true`. That flag
is set once per credential and project, never per machine.

### The Access token

If the server sits behind Cloudflare Access, add `--cf-service-token` and give
the node's Access client id and secret when prompted.

## Start, stop, drain and revoke

All four are on the machine's pane on the Swarm page.

- **Start worker**: the machine is offered work again.
- **Drain**: no new offers; running attempts finish.
- **Stop worker**: no new offers, and every running attempt is stopped. Its
  workspace is copied back, the work is committed and a handoff note is posted
  on the task thread. The task gets a new attempt that starts from that saved
  commit.
- **Revoke machine**: every slot's credential is revoked and the service stops
  polling. Nothing can be minted under the key until a fresh enrollment.

Handoffs caused by Stop (`service_stop`) or by the owner taking over
(`owner_stop`) are not charged to the retry, lineage-time or sandbox-minute
budgets, so stopping a machine repeatedly never exhausts a task. A crash
restart is still charged, so a crash loop stays bounded.

## Restart behaviour

After a service restart, a reboot or `wsl --shutdown`, the service recovers
each slot's interrupted attempt: it saves what the attempt had, reports it
once (a second recovery reports nothing new), and the server gives the task a
fresh attempt from the saved work. An attempt that fails while the service
runs is recovered the same way before the slot takes new work. Idle slots
start polling again. If every poll is refused, or recovery cannot reach the
server, a slot backs off from 30 s up to 300 s instead of looping.

A slot whose recovery is held (for example its sandbox is gone) stays held
until the owner releases it; the other slots and the service keep running,
even when every slot is held.

After a crash, `systemctl --user kill` or a reboot, the attempt's session host
(its guardian) died too and cannot prove its own stop. The restarted service
looks for an outside proof that every process of the lost host is dead:

- `boot_changed`: the machine rebooted since the attempt started (the recorded
  boot id differs from `/proc/sys/kernel/random/boot_id`);
- `cgroup_teardown`: the service runs under systemd and, when it started, its
  unit's cgroup held only itself, and the lost guardian started before it on
  this boot (kernel start times).

With a proof, the service stops the attempt's sandbox (it is kept, never
destroyed) and asks the server to release the attempt
(`POST /v1/worker/lost-attempts`, with the proof). The server fails it as
`lost_after_reboot` (not charged to the retry, lineage-time or sandbox-minute
budgets) or `lost_after_crash` (charged, like any crash) and retries the task
from its original input: the attempt's unsaved work is not salvaged.
`lost-ATTEMPT.json` in the slot's archived state records this, and the slot is
free again. Release the kept sandbox with `service release-sandbox` (below).
Without a proof, or if the server refuses the release, the slot is held for
the owner and the reason is logged.

The slot's aq-git was killed too. The restarted service removes the socket it
left and clears its ownership fence (`aq-git recovery clear-fence`), but only
when no process holds the aq-git state's `owner.lock`; otherwise the slot
retries with backoff. Which claimed sandboxes were preserved is read from the
recorded claims (durable state), so a reboot that empties the runtime directory
changes nothing: a preserved sandbox (its work was never copied back) is never
destroyed by the service, and other claimed sandboxes are still released.
Release a preserved one once you have looked at it:

```sh
aq-worker service release-sandbox --profile local --slot 2 \
  --attempt-id ATTEMPT --reason "inspected; work not needed"
```

It refuses an attempt the slot's recovery still holds for salvage (its sandbox
holds the unsaved work), unless you pass `--force`. A lost attempt's record
(`lost-ATTEMPT.json`) marks its sandbox as kept; the service never destroys
it, even when the attempt had recorded a claim (`claim_unreported`: its work
is then in the archived worktree).

The stop timeout of the unit is the profile's `shutdown_grace_ms` plus 30 s
(a drop-in, `aq-node@PROFILE.service.d/stop.conf`).

## Upgrade

Install the new bundle's binaries over the old ones, then restart the unit:

```sh
for f in aq-worker aq-session-host aq-agent aq-git aq-sandbox-driver; do
  install -m 0755 ~/aq-new-bundle/$f ~/.local/share/agent-q/runtime/$f
done
systemctl --user restart aq-node@local
```

Replace `native.json` too if the build changed its digests: copy the new
bundle's file over `~/.config/agent-q/service/PROFILE/native.json` before the
restart. Do not enroll an enrolled machine again for an upgrade; re-running
the install script for the binaries skips enrollment.

## Uninstall and re-enroll

```sh
aq-worker service uninstall --profile local            # keep configuration and state
aq-worker service uninstall --profile local --purge    # also delete them
```

`--purge` deletes the configuration directory, the `state_root` and
`runtime_root` that `service.toml` names (custom ones too), and nothing else.
It does not revoke the machine's nodes: revoke the machine on the Swarm page.

After a revoke, enroll again with a new code from "Add machine": run
`aq-install.sh` again with the same `--profile` and `--machine-key` and
`--reconfigure`. The service is restarted with the new credentials. After a
purge, `--reconfigure` is not needed. The key can be claimed again only by a
fresh enrollment.

## Install from Add machine

The hosted command targets Ubuntu 24.04 and 26.04 on x86-64, including WSL2
with systemd enabled. It installs `libutempter0` for the bundled tmux when
needed. In **Settings → Framework connection → Add machine** or **Agents →
Swarm**, copy the command into the new PC's WSL terminal. Paste the one-time
code when the worker asks for it. The command downloads the pinned worker
bundle, prepares the rootless sandbox and matching image, then starts one slot.
It can request sudo for missing Ubuntu packages or systemd linger. A fresh WSL
distro needs curl and a running systemd user session.

Enrollment establishes the AQ connection without requiring provider accounts on
that machine. The hosted installer also installs the login receiver described
below. Select the connected worker in Agents → Swarm and use **Push logins** to
install AQ's existing Claude and Codex subscription logins. Manual sessions are
enabled automatically after the first successful delivery.

Separate local accounts remain optional: supply a Claude token with
`AQ_CLAUDE_TOKEN_FILE` when installing, or sign into Codex locally before install.
AQ-managed delivery switches AQ's auth paths to its private profile files and
keeps those personal login files intact.

Before reporting success, the hosted installer checks the daemon's rootless
cgroup-v2 setup and runs a disposable container with the agent's bridge network,
memory/CPU/PID limits and tmux. The test mounts no provider login and invokes no
model. A failure prints the container engine's diagnostic and cleans up the test
container; an enrollment/heartbeat alone does not mean the installation passed.
The remote controls are installed before this check, so a failing machine can
return its diagnostic through AQ without copying terminal output.

The command can be rerun with the same bundle after an interrupted image pull.
It resumes an unfinished enrollment or reuses the saved enrollment. Use a fresh
distro or the regular upgrade procedure if another worker runtime already owns
the installation paths.
Configuration fixes can be delivered in the pinned bootstrap while reusing
the existing runtime bundle and enrollment. Rerunning the latest command
repairs the manual-session section written by an earlier tokenless install.

### Prepare the download on the server

Run `packaging/service/prepare-worker-install.py --help`. Give it the active
service TOML, the matching worker binary and runtime directory, and the
installed Podman distribution directory. Optionally supply the pinned Codex
helper directory with `--codex-helpers`. On an Ubuntu build host with a
dynamically linked tmux, install `patchelf` so the builder can package tmux
with its matching terminfo library. The builder verifies the pinned host
tools and image manifest before packaging. It does not distribute enrollment
files or subscription logins.
The output contains the checksummed installer, checksummed bundle and an nginx
configuration.

Start nginx using the generated file as an unprivileged service. For example,
`nginx -p /absolute/output/ -c nginx.conf -g 'daemon off;'`. The default
configuration listens at `127.0.0.1:28443` and proxies the worker API on
`127.0.0.1:28081`. It serves only the pinned registry manifest and its blobs,
using the existing loopback registry as source. Leave the AQ admin listener
separate.

- **Tailscale**. Run `tailscale serve --bg --https=8443 http://127.0.0.1:28443`
  on the server host. On Windows, use its PowerShell and the Windows Tailscale
  client. Complete the tailnet HTTPS approval if prompted. Copy the private
  HTTPS address reported by Serve.
- **LAN**. On a host with an address reachable from the LAN, generate the config
  with `--listen PRIVATE_IPV4:28443 --tls-cert CERT --tls-key KEY`. Set up LAN
  DNS for the hostname in the certificate. Use a certificate trusted by the
  worker binary and curl. For a WSL server, the Windows host must route the
  chosen LAN address to this worker ingress. A LAN reverse proxy with a trusted
  certificate can also use the default loopback ingress as its upstream.

Set `VITE_AQ_WORKER_INSTALL_URL` to the resulting private HTTPS origin and
`VITE_AQ_WORKER_INSTALL_SHA256` to the builder's `installer_sha256` when building
the dashboard. **Add machine** copies the pinned command and generates a
separate single-use code. For each new bundle, use its new installer checksum
in the dashboard build. Verify `/healthz` from the target WSL terminal before
enrolling the first machine.


### Push saved logins to a connected worker

The hosted installer installs a user timer, `aq-login-sync-<profile>.timer`, alongside
`aq-node@<profile>.service`. In Agents → Swarm select a worker, choose Claude and/or
Codex under Server logins, then select **Push logins**. Delivery is queued while
the worker is disconnected or its machine has active agents. The dashboard shows
queued, installed, or failed status. A failed installation can be retried with a
new push. An uncertain request is retried with the same operation identity.

A worker installed before this receiver was shipped needs the current hosted
install command once. It retains its enrollment, configured slots and runtime.
Subsequent login pushes require no terminal command or new enrollment code.

The receiver uses each enrolled slot's credential over the installation's pinned
HTTPS origin and refuses redirects. The server serves only explicitly requested,
permitted subscription logins, using its current versioned material owner. API
keys and other credentials are excluded. Neither the dashboard nor the SQL queue
receives secret material. Worker files live in the profile's `provider-auth`
directory (0700), with individual files written atomically as 0600. This changes
AQ's configured auth paths without overwriting personal Claude/Codex login files.
New provider support restarts the worker only while the machine is idle; updates
to material at existing paths are available to the next agent without a restart.

Inspect the receiver with `systemctl --user status aq-login-sync-local.timer` and
`journalctl --user -u aq-login-sync-local.service`. Diagnostics deliberately omit
HTTP response bodies and credentials. To stop future delivery on this installation,
disable its timer; revoke/remove the worker in AQ to reject future authenticated
requests. Existing provider sessions still require their normal Stop agent control.

### Remote diagnostics

The updated receiver also polls owner-requested diagnostics. Select a worker and
use **Run checks** to test its rootless daemon, cgroup settings, pinned image,
container creation/start and tmux through the same Docker API policy as an agent.
The dashboard receives the failing phase and engine diagnostic automatically;
terminal output does not need to be copied between machines. Recent-session
metadata reports whether a sandbox was created, without collecting agent output
or provider files. Tests use a unique disposable container/volume and no model.

Older receivers need the current hosted installer once. Afterwards **Update
controls** queues a receiver update using the dashboard's pinned installer hash.
The receiver downloads that script from its enrolled HTTPS origin, verifies it,
and atomically replaces only its own embedded Python code. It does not execute
the downloaded installer shell, change enrollment or replace runtime binaries.
Updates and diagnostic receipts are durable and retried by identity. No generic
remote shell command is accepted by this protocol.
