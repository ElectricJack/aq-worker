# F1a host: blocked implementation checkpoint

Task: `crisp-nexus-12`. Inspected source:
`1a7ebca0a110db46d1495ca35de1338e253411c3`.

No taskless host release or consumer pin was produced. The existing
`aq-session-host` 0.1.5 and `aq-session-protocol` 0.1.0 remain unchanged.
Do not use this checkpoint as qualification for central supervisor launches.

## Source findings

- `crates/aq-session-host/src/lib.rs:148-157`: `HarnessAdapter` takes a
  `LaunchManifest`; it has no subject-neutral preparation entrypoint.
- `crates/aq-session-host/src/lib.rs:342-391`: `SealedLaunch` retains that
  manifest and derives the permit/session binding from it.
- `crates/aq-session-host/src/lib.rs:459-525`: public launch paths validate
  `LaunchManifest` before preparing the adapter.
- `crates/aq-session-protocol/src/lib.rs:218-280`: task/attempt bindings are
  required, and nil task IDs are rejected even with a Central session identity.
- `crates/aq-session-host/src/lib.rs:791-850`: recovery and existing-guardian
  observation already exist, but use the task-bound sealed configuration.

## Unresolved prerequisites

The task cites `AQ-17-central-authority-core.md:47-55` and requests a contract
owner decision. The referenced document was not found in this checkout;
`aq file read --path projects/aq-worker/plans/AQ-17-central-authority-core.md
--offset 1 --limit 100` returned file not found. The supervisor was asked for
the authoritative plan and confirmation whether this task owns the new
protocol contract or consumes another upstream contract. Message:
`msg-4bd10221e310473caf9994acf3865473`. No reply was present when the injected
session inbox was checked after the inactivity reminder.

The required `cargo test --locked --workspace supervisor_session` was not run.
`aq test --help` invoked the pytest resource wrapper, so the Rust validation
invocation was included in the supervisor request. This is not a test pass.

The final submission instruction `git merge origin/dev` failed with
`merge: origin/dev - not something we can merge`; no local remote-tracking
`origin/dev` exists. `origin/HEAD` points to `origin/main`. No alternative
integration branch was substituted.

## Remaining delivery

Supply the plan and resolve contract ownership, then implement and qualify
the additive subject-neutral HostPlan/adapter path without weakening v1 or
inventing task IDs. Qualify permit binding, durable callbacks, stopped-writer
proof, restart observation, and unchanged legacy launches. Publish the pinned
release and replace this blocked checkpoint with exact consumer artifacts and
test evidence. The server host container and dispatch remain out of scope.
