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
new code. The slot credentials are written only once the claim succeeds.

### The machine key

The machine key is the one name for the machine: the Swarm page shows it, and
start, stop, drain and revoke act on it. It is `--machine-key`, or else the
lower-cased host name. It must match `[a-z0-9][a-z0-9-]{0,62}`. Slot 1 claims it
on the server; the other slots are minted under it.

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

After a revoke, enroll again with a new code from "Add machine": run
`aq-install.sh` again with the same `--profile` and `--machine-key` and
`--reconfigure`. The service is restarted with the new credentials. After a
purge, `--reconfigure` is not needed. The key can be claimed again only by a
fresh enrollment.
