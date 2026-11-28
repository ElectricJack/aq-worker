# Install the aq-worker service (Linux and WSL2)

This installs `aq-worker` as an always-on systemd user service with one or more
nodes, each running a server-controlled count of agent slots, enrolled with a single-use code. It is separate from the per-profile
container worker in [install-worker.md](install-worker.md). macOS is not
supported yet; the script refuses it.

## Prerequisites

- Linux, or WSL2 with systemd. For WSL2 follow "Prerequisites" in
  [install-worker.md](install-worker.md) (`/etc/wsl.conf` with `[boot]
  systemd=true`, then `wsl --shutdown` once).
- Permission to enable systemd linger. The installer enables and verifies it,
  using sudo if needed, so the service starts at boot without a login session.
- On WSL2, Windows PowerShell interop and `WSL_DISTRO_NAME` must be available.
  The installer registers the Windows sign-in task and keepalive automatically
  using the bundled `aq-wsl-autostart.ps1`. This service uses rootless Podman
  and does not need Docker Desktop. Windows starts the distro at **sign-in**;
  it does not run the worker before anyone signs in.
- WSL2 host load: see "Windows host load (WSL2)" in
  [install-worker.md](install-worker.md). The script prints the probe command
  when the host-load file is missing and never runs it.
- The rootless sandbox daemon: either its socket already exists at
  `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock`, or pass `--sandbox-bundle
  podman-linux-amd64.tar.gz` and the script installs and starts it. Without
  either it exits with code 4.
- A single-use enrollment code, created from the Swarm page ("Add or update machine").

## The bundle directory

`--bundle DIR` holds:

- the four binaries `aq-worker`, `aq-session-host`, `aq-agent` and `aq-git`
  (the sandbox driver is `aq-worker` itself: `aq-worker sandbox-harness`);
- `native.json`.

Copy `native.json` from an enrolled machine of the same build. `native.json` pins exact
image and tool digests, so a file from another build will not match the binaries.
`--native-template` overrides the default.

A worker lists no repositories. It runs work for any repository the server's offer names:
the repository (its id, identity and binding generation come with the offer's source) is
registered with the slot's `aq-git` the first time the slot sees it, and again from the
attempt's recorded offer when the service restarts and recovers the attempt. A changed
binding generation is refused. Sibling workspace sources come from the offer's workspace
plan through node Git and need no entry either. Hand-started sessions take their repository
from the server's session spec. Adding a project needs no change on any worker.

`service.toml` no longer has `repositories` or `[manual.source_slugs]`, and
`aq-worker service enroll` has no `--repositories`. An installed `service.toml` that
still has either is rewritten once when the worker loads it, without them and with every
other setting kept (the file as it was stays beside it as
`service.toml.before-repository-removal`), so a remote update needs no edit. A document
parsed without that rewrite (a hand-written one) is refused with a message that says to
remove them. Object format: the offer's binding does not carry one, so the worker registers
repositories as `sha1`.

### After a failed attempt

A slot whose attempt failed is settled at once and comes up again. It reports to the
server (heartbeat, inventory) while it waits and only asks for no offer until the pause
has passed. Optional `[retry]` keys in `service.toml` set the pauses, in milliseconds:

| Key | Default | Meaning |
| --- | --- | --- |
| `offer_pause_ms` | 3000 | After the server refused the slot (409, another 4xx, 503), one offer failed before its launch (its source, a local `aq-git` refusal), or the server was not reached. |
| `offer_pause_max_ms` | 30000 | Ceiling of that pause. It doubles while the same kind of failure repeats and starts again after another kind or a finished attempt. |
| `fault_backoff_start_ms` | 30000 | After any other failure (the sandbox, the disk, a refused credential) and after a failed recovery. Doubles until an attempt finishes. |
| `fault_backoff_max_ms` | 300000 | Ceiling of that back-off. |

A slot that fails as it starts cannot report; it waits the same time without reporting.

## Install

```sh
packaging/service/aq-install.sh --server https://aq.example --bundle ~/aq-bundle \
  --profile local --nodes 2 --machine-key my-box
```

Options: `--profile` (default `local`; lower-case letters, digits and `-`, at
most 32 characters), `--nodes` (default 1, at most 8; `--slots` is its legacy alias), `--name`,
`--machine-key`, `--cf-service-token`, `--native-template`,
`--sandbox-bundle` and `--reconfigure`.

The script refuses to replace a `~/.local/bin/aq-worker` that is not its own
link, and warns when the container worker `aq-worker@local` is running too.

The script, in order:

1. checks the platform, systemd and the machine key, then enables and verifies
   linger; on WSL2 it also installs the Windows sign-in task and starts its
   keepalive. A startup setup failure stops the installer before enrollment;
2. makes sure the sandbox daemon is running and enabled for the next boot,
   even when its socket already exists;
3. installs the four binaries into `~/.local/share/agent-q/runtime/` and links
   `~/.local/bin/aq-worker`;
4. runs `aq-worker service enroll`, which prompts for the code. A profile that
   is already enrolled is skipped (pass `--reconfigure` to enroll it again);
5. runs `aq-worker service install` (writes the user unit `aq-node@PROFILE`
   and starts it, or restarts it when it is already running);
6. prints the Windows probe command on WSL2 if needed;
7. prints `aq-worker service status`.

Rerunning the installer on an enrolled profile also repairs boot startup without
consuming another enrollment code. To check an existing installation:

```sh
loginctl show-user "$USER" -p Linger --value       # yes
systemctl --user is-enabled aq-sandbox-podman.service aq-node@local.service
systemctl --user status aq-node@local.service
```

Use the actual profile instead of `local`. For the Windows task and removal
commands, see "Autostart" in [install-worker.md](install-worker.md).

The enrollment code is typed at the prompt, or piped as the first stdin line.
With `--cf-service-token` the next two prompts (or lines) are the node's Access
client id and secret. Nothing secret is ever put in an argument, the
environment, a log or a unit file. The code works once.

`enroll` checks every argument (the server origin, the native template)
before it reads the code, so a typo never uses it up. The
server must be `https`; plain `http` is accepted only to `127.0.0.1` or `::1`.
If the machine key claim fails after the code was redeemed (for example the key
is in use), run the same command again: it resumes at the claim and needs no
new code. If Cloudflare Access refused the stored token at the claim, resume
with `--reconfigure --cf-service-token` and the new Access client id and secret.
The node credentials are written only once the claim succeeds, all of them
together. If a run stopped after the claim but before it saved the nodes, the
rerun says so: revoke the machine and enroll again with a new code.

### The machine key

The machine key is the one name for the machine: the Swarm page shows it, and
start, stop, drain and revoke act on it. It is `--machine-key`, or else the
lower-cased host name. It must match `[a-z0-9][a-z0-9-]{0,62}`. Node 1 claims it
on the server; the other nodes are minted under it. The machine's label is the
key too, unless `--name` gives another.

### Where the service keeps its files

- Configuration: `~/.config/agent-q/service/PROFILE/` (`service.toml`,
  `machine.json`, `native.json`, one `slots/N/worker.toml` per enrolled node).
- Durable state: `state_root` in `service.toml`, by default
  `~/.local/state/agent-q/service/PROFILE/`: each slot's journal, Git state,
  materials and archives. Today nothing deletes old archives or kept sandboxes;
  release them by hand (below). Target (2026-11-13, implementation pending): both are rolling
  sets under aq-server
  [architecture 0006](../../../aq-server/docs/architecture/0006-records-are-rolling.md) and SBX22: the most recent 20 archives per slot, none
  older than 14 days, and the most recent `preserve.max_count` kept sandboxes
  (default 2 per machine), none older than `preserve.max_age` (default 3
  days); the service deletes older ones itself.
- Runtime files: every Unix socket (aq-git's, tmux's, the session host's) and
  the session host's per-launch directories live under a short runtime root,
  `$XDG_RUNTIME_DIR/aq/PROFILE/` (for example `/run/user/1000/aq/local/`), or
  `/tmp/aq-UID/PROFILE/` when `XDG_RUNTIME_DIR` is unset. Each slot has
  `K/git.sock` and `K/host/` for node K's first agent; later agents use
  `K-N/git.sock` and `K-N/host/`. The service creates the root
  0700 and refuses one that is a symlink, belongs to another user or is open to
  others. These files do not survive a reboot, and need not.

A Unix socket path holds at most 103 bytes here, so `service.toml` is refused
at load (and `enroll` refuses before it reads the code) when a slot's socket
path would be longer; the error names the path and its length. With the
default runtime root this only happens for a long profile name (more than about
20 characters under `/run/user/1000`). Use a shorter profile, or set
`runtime_root = "/absolute/short/dir"` in `service.toml`: its parent must be
owned by you and writable only by you.

Agent capacity is a server Setting per node. Increasing it does not mint or enroll another
node. Existing `service.toml` files with `slots = N` still describe N nodes; new files say
`nodes = N`. A binary update keeps every node. See [multi-agent nodes](multi-agent-nodes.md)
for independent recovery, shared manual/queued capacity and explicit node retirement.

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
- **Revoke machine**: every node's credential is revoked and the service stops
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

Target (2026-11-13, implementation pending): after a crash, kill or reboot the
service tries once to salvage the lost attempt's unsaved work from the stopped
sandbox and offers it to the retry as its starting point, told that it
continues from interrupted, possibly half-finished work. After a successful
salvage the sandbox is deleted at once. A sandbox whose salvage failed is kept
only for inspection: at most `preserve.max_count` per machine (default 2),
each for at most `preserve.max_age` (default 3 days), then the service deletes
it. Both are settings. `service release-sandbox` then only deletes one sooner.
A planned restart or stop is unchanged. Details: worker spec 0001 §16.
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
aq-worker service release-sandbox --profile local --node 2 --slot 1 \
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
for f in aq-worker aq-session-host aq-agent aq-git; do
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

After a revoke, enroll again with a new code from "Add or update machine": run
`aq-install.sh` again with the same `--profile` and `--machine-key` and
`--reconfigure`. The service is restarted with the new credentials. After a
purge, `--reconfigure` is not needed. The key can be claimed again only by a
fresh enrollment.

## Install from Add or update machine

The hosted command targets Ubuntu 24.04 and 26.04 on x86-64, including WSL2
with systemd enabled. It installs `libutempter0` for the bundled tmux when
needed. In **Settings → Framework connection → Add or update machine** or **Agents →
Swarm**, copy the command into the new PC's WSL terminal. Paste the one-time
code when the worker asks for it. The command downloads the pinned worker
bundle, prepares the rootless sandbox and matching image, then starts one slot.
It configures boot startup, including the WSL sign-in task, and can request sudo
for missing Ubuntu packages or systemd linger. A fresh WSL distro needs curl,
Windows PowerShell interop and a running systemd user session. No separate
Windows autostart command is needed for this installer.

Enrollment establishes the AQ connection without requiring provider accounts on
that machine. The hosted installer also installs the login receiver described
below. Select the connected worker in Agents → Swarm and use **Push Claude
token** to install AQ's saved Claude subscription token. Manual sessions are
enabled automatically after the first successful delivery.

A Claude token can instead be supplied with `AQ_CLAUDE_TOKEN_FILE` (an absolute
path) when installing. AQ-managed delivery writes AQ's private profile file and
keeps a personal login file intact.

No Codex login is installed on a worker. The server's login store holds the one
copy of the owner's Codex (ChatGPT) login. Every Codex agent, queued or started
by hand, leases the newest stored version when it starts and the worker writes a
login that Codex refreshed back to the store, so no copy is left with a refresh
token that another copy has already used. A hand-started session is checked
every `login_check_secs` (default 60, `[manual.codex]` in `service.toml`) and
once more when it stops; a newer stored version is installed into the running
sandbox, where Codex picks it up. The owner puts their current login into the
store with `aq-server import-login` on the server host. How old a stored login
may be when it is leased is the owner's Setting (Settings → Providers); the
server refuses an older one and the agent shows the reason. `[manual.codex]
auth_file` no longer exists: remove the key from an older `service.toml`, or the
worker refuses the file (`unknown field `auth_file``).

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
the existing runtime bundle and enrollment.

Manual Claude sessions log in with the subscription token file named by
`claude_token_file` in the `[manual]` section of `service.toml` (an absolute
path; required whenever `[manual]` is present). The installer and Push Claude
token write it. The worker runs whatever model and reasoning effort an offer names:
there is no qualification measurement and no `[measurement]` section. A
`service.toml` that still has one (a machine updated remotely keeps the file its
installer wrote) is rewritten once by the worker when it loads the file: the
section is removed, and if the profile has a `[manual]` section without
`claude_token_file`, the old `token_file` value is set there. Everything else
is kept, and the file as it was stays beside it as
`service.toml.before-measurement-removal`. If `[manual]` is present and no token
file is named anywhere, the service then stops with `missing field
`claude_token_file``: add the key by hand or use Push Claude token.

### Prepare the download on the server

Run `packaging/service/prepare-worker-install.py --help`. Give it the active
service TOML, the matching worker binary and runtime directory, and the
installed Podman distribution directory. Supply the pinned Codex helper
directory with `--codex-helpers` when the native template names a Codex
runtime bundle. On an Ubuntu build host with a
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

Once that gateway runs, publish each later bundle with `--gateway-url` (for
example `http://127.0.0.1:28443`, the default listener). The builder then
replaces the image routes of the output's `nginx.conf` with the manifest and
blobs of the template image the bundle's `native.json` names, checks and
reloads that nginx (`nginx -p OUTPUT/ -c nginx.conf`, or `--nginx PATH`) and
GETs each path through the gateway. If any is not served it publishes nothing
and exits non-zero, so no machine is told to update to an image it cannot pull.
`scripts/ms1-worker-deploy.py` always passes it (`gateway_url` and `nginx` in
its configuration).

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
the dashboard. **Add or update machine** copies the pinned command and generates a
separate single-use code. For each new bundle, use its new installer checksum
in the dashboard build. Verify `/healthz` from the target WSL terminal before
enrolling the first machine.


### Push saved logins to a connected worker

The hosted installer installs a user timer, `aq-login-sync-<profile>.timer`, alongside
`aq-node@<profile>.service`. In Agents → Swarm select a worker and select **Push
Claude token** under Claude token. Only the Claude token is delivered this way;
a Codex login is leased per agent (see above). Delivery is queued while
the worker is disconnected or its machine has active agents. The dashboard shows
queued, installed, or failed status. A failed installation can be retried with a
new push. An uncertain request is retried with the same operation identity.

A worker installed before this receiver was shipped needs the current hosted
install command once. It retains its enrollment, configured node count and runtime.
Subsequent login pushes require no terminal command or new enrollment code.

The receiver uses each enrolled slot's credential over the installation's pinned
HTTPS origin and refuses redirects. The server serves only the explicitly requested,
permitted Claude subscription token, using its current versioned material owner. API
keys and other credentials are excluded. Neither the dashboard nor the SQL queue
receives secret material. Worker files live in the profile's `provider-auth`
directory (0700), with individual files written atomically as 0600. This changes
AQ's configured token path without overwriting a personal Claude login file.
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
and atomically replaces its own embedded Python code. It then downloads the
worker bundle the script pins and verifies it against that checksum. It unpacks
the bundle where a full install puts it and pulls the image named by the
bundle's native template, by its digest, into AQ's sandbox daemon. Only then does
it replace `aq-worker` and the profile's native template, and restart the worker.
So a remotely updated machine launches the image its bundle names, as a freshly
installed one does. If the pull fails, the worker and template are left as they
were, and the report gives the engine's reason. The report names the image and
whether it was pulled.
It does not execute the downloaded installer shell or change enrollment, and the
bundle's other runtime programs change only with a full install.
Updates and diagnostic receipts are durable and retried by identity. No generic
remote shell command is accepted by this protocol.

Each inventory report (`POST /v1/worker/profiled/inventory-v2`) of a docker-local
worker carries the header `aq-sandbox-template: sha256:<hex>`. That is the digest of
the template image the worker launches, as its sandbox daemon resolved it when the
worker started. The header is sent only when that image is present locally under
that digest. It travels beside the pinned `InventoryV2` document, the way
`aq-node-slots` travels beside `Register`, so the server can compare it with the
image its environment's definition names.

The selected-worker update command uses `--controls-only`. It replaces the
existing receiver and restarts its timer without native-template comparison,
runtime replacement, enrollment, image preparation or sandbox validation. This
keeps remote diagnosis available when those checks are what failed. Bootstrap
errors are returned to the queued AQ diagnostic when its enrollment is usable;
control polling can also recover the enrolled slot list from a malformed service
configuration. Full installations still run all sandbox checks before success.

### Rootless bridge networking dependency

Fresh hosted installs require the Ubuntu `iptables` package. A rootless
container without networking can pass while the agent's bridge network fails
with `netavark: iptables: No such file or directory`.

For connected workers, the fixed sandbox check recognizes that exact startup
error and restores checksum-pinned AQ networking tools under the existing
AQ sandbox runtime. The package includes iptables, its private shared libraries
and extensions, package versions and copyright notices. It is built reproducibly
by `prepare-network-tools.py` on Ubuntu amd64 and verified on Ubuntu 26.

Repair changes only the AQ sandbox user service's PATH through its own drop-in.
It checks `KillMode=process` before restarting the API daemon, preserving the
container supervision boundary. It does not invoke sudo, install system packages
or change global firewall configuration. The iptables wrapper uses its private
libraries/extensions and an AQ-scoped lock file. Other diagnostic failures do not
trigger this repair. The complete sandbox probe is repeated after restoration.
