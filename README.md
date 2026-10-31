# aq-worker

A standalone Rust worker-node framework for Agent Q. A node provides a bounded amount of agent execution capacity, prepares workspaces on demand, and maintains a bounded set of project caches. Nodes connect outward to the central daemon to enroll, heartbeat, receive assignments and messages, and hand off results.

Supervisor agents live centrally. The node runtime manages processes and local resources; it does not plan tasks, run supervisor agents, or decide integration policy. Repository operations go through the separately versioned aq-git interface.

This repository contains the extracted foundation connector and an execution-protocol 2 candidate with a durable local journal and a synthetic process adapter. The existing version-1 enrollment, configuration, heartbeat and probe contract is preserved. This is local component implementation evidence; production harnesses, source handoff, deployment and product gates remain separate work.

- [Worker framework design](docs/specs/0001-worker-framework.md)
- [Worker execution recovery and agent access](docs/specs/0002-execution-and-agent-access.md)
- [Worker protocol ownership](protocol/README.md)
- [Shared execution and recovery](../agent-q/docs/architecture/0002-execution-and-recovery.md)
- [AQ ecosystem and releases](../agent-q/docs/architecture/0001-ecosystem-and-releases.md)
- [Agent Q foundation](../agent-q/docs/specs/0001-infrastructure-foundation.md)
- [aq-git design](../aq-git/docs/specs/0001-aq-git.md)

Sibling links assume side-by-side checkouts. They are documentation references, not build dependencies. The worker's core tests will use a local fake coordinator and deterministic harness; real container and harness compatibility tests will be separate suites.

Run `cargo test --workspace --locked --all-features` and
`cargo fmt --all -- --check` without sibling repositories or external services.
The foundation connector retains `aq-worker enroll`, `run`, and `status`.
`aq-fixture-agent` is a synthetic deterministic test process, not a real agent harness.
See [implementation evidence](docs/implementation/README.md).

Run a synthetic assignment against a separately running compatible daemon:

```sh
cargo build --locked --bins
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
