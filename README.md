# aq-worker

aq-worker is the worker-node component of AQ v2. A node provides a bounded amount of agent execution capacity: it enrolls with the AQ server, connects outward to it, receives assignments, runs each agent in a local Docker sandbox, prepares workspaces through aq-git, and reports results and handoffs back. This repository is the `aq-worker/` submodule of the umbrella repository [aq-platform/agent-queue](https://github.com/aq-platform/agent-queue), which has the overview and the other five components.

Supervisor agents and scheduling live centrally. The node runtime manages processes and local resources; it does not plan tasks, run supervisor agents, or decide integration policy. Repository operations go through the separately versioned aq-git interface.

What the code does today:

- `aq-worker service enroll|install|uninstall|status` enrolls a machine and runs it as a systemd user service (Linux and WSL2). `aq-worker run --service-config` is the production path: several slots of docker-local work per node, with observation stores under the service state root. See [installing the service](docs/operations/install-service.md).
- `aq-worker run-native`, `check-native`, `reconcile-native` and `release-native-sandbox` drive and inspect single native attempts. A Claude executor runs in docker-local sandboxes and a Codex executor is present. The Gemini and OpenCode adapters have live tests that are `#[ignore]`d by default.
- `aq-worker run-checks` claims and runs server check attempts in the same sandbox.
- `aq-worker run --controller-config` is the legacy controller path. It still has no registration and issuance provider installed, so it refuses registration and keeps only the foundation heartbeat, with admission fenced. See [controller configuration and limits](docs/implementation/B5a.md).
- The `synthetic-runtime` feature adds `aq-worker run-execution`, a protocol-2 execution runtime with a durable local journal that drives `aq-fixture-agent`, a deterministic test process and not a real agent harness. It is a test harness and is never enabled by a plain build.

Per-change evidence is in [docs/implementation](docs/implementation/README.md). That evidence is historical: each record is bound to the source revision it names.

- [Worker framework design](docs/specs/0001-worker-framework.md)
- [Worker execution recovery and agent access](docs/specs/0002-execution-and-agent-access.md)
- [Profiled tmux sessions and outbound terminal access](docs/specs/0003-profiled-tmux-sessions.md) (partly implemented; its status line says which parts)
- [Session observation, transcript accounting and suspension](docs/specs/0004-session-observation-and-suspension.md) (implementation design; includes cross-node continuation startup; read its status line)
- [Worker protocol ownership](protocol/README.md)
- [Qualified opt-in Docker settings for Codex workspace tools](docs/operations/codex-docker-security.md)
- [Installing the worker as a service](docs/operations/install-service.md)
- [Local source workspaces](docs/source-workspaces.md)
- [Resumable source transfer candidate](docs/source-transfers.md)
- [Scoped agent client and protected provisioning](crates/aq-agent-client/README.md)
- [Standalone pinned native interactive runtime image](packaging/native-runtime/README.md)
- [Observation engine and transport owner integration](docs/implementation/WRK-15-integration.md) (historical component qualification record)
- [Shared execution and recovery](../aq-server/docs/architecture/0002-execution-and-recovery.md)
- [AQ ecosystem and releases](../aq-server/docs/architecture/0001-ecosystem-and-releases.md)
- [Agent Q foundation](../aq-server/docs/specs/0001-infrastructure-foundation.md)
- [aq-git design](../aq-git/docs/specs/0001-aq-git.md)

Sibling links (`../aq-server/...`, `../aq-git/...`) resolve in an umbrella checkout, where the components sit side by side. They are documentation references, not build dependencies. The workspace's tests use local fake coordinators and deterministic harnesses; live tests that need a daemon, Docker images or provider credentials are `#[ignore]`d.

Run `cargo test --workspace --locked --all-features` and
`cargo fmt -- --check` from the repository root
without sibling repositories or external services. The taskless supervisor host
qualification tests require Linux with `/usr/bin/tmux` installed (CI installs
tmux explicitly); they exercise real local processes without a provider. CI also
pulls a pinned Ubuntu image for the numeric-UID security probe, so a full local
run needs Docker (see `.github/workflows/check.yml`). `cargo build --locked --bins`
builds `aq-worker`, `aq-fixture-agent`, `aq-session-host` and `aq-agent`.
The formatting command checks every member of this virtual workspace, including
its tests and examples. Omit `-- --check` to apply formatting.

Use this workspace-only formatting command in AQ's nested `.aq/worktrees/`
checkouts too. `cargo fmt --all` also traverses excluded local path dependencies
under `vendor/`. In a nested checkout, Cargo
can associate those packages with the outer checkout's workspace and fail
metadata discovery before formatting. An explicit `--manifest-path` alone does
not prevent that extra traversal when combined with `--all`.

To check formatting of vendored dependencies as well, extract a committed tree
outside any other Cargo workspace and run the broader check there:

```sh
standalone_dir=$(mktemp -d /tmp/aq-worker-standalone.XXXXXX)
git archive HEAD | tar -x -C "$standalone_dir"
(
  cd "$standalone_dir"
  cargo fmt --all -- --check
  cargo build --workspace --locked --all-features --jobs 2
)
```

This checks the committed revision; commit local edits before using it to verify
them. All local build dependencies are inside that extracted tree. Keep the
temporary directory for inspection, then remove it when finished.

`aq-worker enroll`, `run` and `status` come from the foundation connector. `run` takes
either `--service-config /absolute/service.toml` (the service path above) or the
legacy `--controller-config /absolute/node.json`; the two conflict. The legacy
path logs the registration refusal and keeps foundation probe/heartbeat/reconnect
active with admission fenced, and it cannot execute sandboxes.
`aq-fixture-agent` is a synthetic deterministic test process, not a real agent harness.
See [implementation evidence](docs/implementation/README.md).

Run a synthetic assignment against a separately running compatible daemon. The
`run-execution` subcommand exists only in a build with the `synthetic-runtime`
feature:

```sh
cargo build --locked -p aq-worker --bins --features synthetic-runtime
target/debug/aq-worker --config worker.toml run-execution \
  --state-dir state --fixture-agent target/debug/aq-fixture-agent \
  --exit-after-result --idle-timeout-secs 60
```

Registration emits JSON containing the node/boot identity so a development
fixture scheduler can supply a bound offer. Local test switches
`--drop-acceptance-response-once` and `--drop-report-response-once` exercise
identical-request replay. They discard one received acknowledgement, preserving
the committed server action. The synthetic process's `AQ_FIXTURE_HOLD=1`
injection waits for a `release` file inside its launch directory; cancellation
and grant-expiry tests prove that control continues while it is blocked.

The synthetic runtime uses the `linux-subreaper-guardian-v1` ownership profile.
A per-launch guardian survives a runtime process crash in the same PID namespace
and independently enforces its grant. It stops and reaps adopted descendants,
including double-forked processes that call `setsid`. Restart stops and preserves
old execution; live-session reattachment is explicitly unsupported. If the
guardian or its proof is lost, reservations remain held. See the
[profile, recovery and qualification limits](docs/implementation/worker-recovery.md).
For a container running worker processes as children, use Docker `--init` or
Compose `init: true` so exited orphan guardians can be reaped.
