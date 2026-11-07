# worker-machine v1

Wire contract for machine load reports (spec 0007).

- Route: `POST /v1/worker/machine`
- Auth: the node bearer credential, as for the heartbeat. Success is `200 {"recorded": true}`. Invalid input is a `400`.
- Cadence: every 15 s from `aq-worker run`.
- Body: a `MachineReport` (`schema.json`). Unknown fields are rejected.

Fields and units:

- `machine.key`: lower-case host key, `[a-z0-9-]`, 1-63 characters. Nodes that report the same key form one machine. `machine.label` is the display name; `machine.kind` is `local` or `cloud`.
- `observed_at_ms`: the worker's clock, in milliseconds. The server uses its own receive time for freshness.
- `source`: `linux` for load read from `/proc`, `windows_host` when the Windows host probe file was fresh.
- `host_probe`: `not_applicable` (not WSL2), `fresh`, `stale` or `missing`.
- `cpu_busy_bps`: CPU busy, in basis points (0-10000).
- `memory_total_bytes`, `memory_used_bytes`: bytes. Used is not above total.
- `disk_total_bytes`, `disk_free_bytes`: both present or both absent.

A report is fresh for 60 s after it is received. The 15-minute average is the mean of the samples received in the last 900 000 ms.

The worker pins this directory by digest (`protocol/pinned/worker-machine/source.json`).
