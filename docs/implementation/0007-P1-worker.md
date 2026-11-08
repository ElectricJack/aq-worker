# Swarm phase 1: workers as services (worker side)

Spec: aq-server `docs/specs/0007-swarm.md` (§5.4, §6, §7.2, §7.3, §8, §10 phase 1). Plan: aq-server `docs/superpowers/plans/2026-11-07-swarm-p1-workers-as-services.md`. Branch `swarm/p1-worker`. Operator guide: `docs/operations/install-service.md`.

## What was built

- **W0.** The `aq-worker` binary and its bin-only modules moved under `src/bin/aq-worker/`, so they no longer count against the lib line ratchet.
- **W2.** `run_until_attempt`: one registration, then poll until an offer arrives; pausable.
- **W3.** Sandbox driver handoff: a marker file stops Claude, copies the workspace back and claims a handoff note.
- **W4.** `salvage_interrupted`: after a restart, an interrupted attempt is reported as a failed handoff result with its saved workspace. It is idempotent, and it refuses a workspace holding an agent credential or the Claude token shape.
- **W5.** Cloudflare Access service-token headers on the Claude executor's relay.
- **W6 to W8.** New crate `aq-node-service`: service config, state layout, server link and aq-git child (W6); the slot supervisor with handoff, renderer switch, backoff and graceful stop (W7); the systemd user unit layer (W8, macOS reserved for phase 5).
- **W9.** CLI glue: `run --service-config`, `service enroll|install|uninstall|status|measure`, `machine.json` and machine reports.
- **W10.** The qualification measurement (the `remeasure.sh` procedure) and its renewal submission.
- **W11.** `packaging/service/aq-install.sh` and `docs/operations/install-service.md`.
- Room-making moves: `aq-finite-responses` and `aq-handoff-note` crates.

## Install script

Local bundle only. Enrolled machines are allowed automatically, so there is no allow-list step. The script never reads a secret: `aq-worker service enroll` prompts for the code itself. It prints the Windows host-load probe command and never runs it. macOS is refused with a clear message. Tests (`packaging/service/tests/test_install.py`) use a temporary HOME and stub `systemctl`, `loginctl`, `ps`, `uname` and `aq-worker`.

## Budgets

aq-worker lib 70,993 (was 71,389). Test binaries 105. `aq-node-service`, `aq-handoff-note` and `aq-finite-responses` are far under 40,000.

## Verified only live (acceptance L)

- `NativeSlotBackend::run_attempt` and `recover` against a live server and docker.
- End-to-end salvage (a successor from the salvaged commit) and the server accepting uploads and reports under the original boot id after re-registration.
- `service install` on a real systemd, and `aq-install.sh` on a real machine.
- `service measure` against a live template and token, then `measured()` on its output, and the recorded server pin.
- `SessionHost::recover` and the stop proof after a kernel reboot.

## Known gaps

- After a restart the leased Claude token is gone, so salvage scans for token shapes, not the exact token.
- A claim file submitted and the sandbox destroyed before the node ingested it leaves the attempt Held (`sandbox_absent`).
