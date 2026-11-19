---
title: "Observations are one node-wide store driven by the node service, not one store per launch"
author: Jack
date: 2026-11-18
branch: feat/node-observation-store
cost: { minutes: 30, tokens: 41974 }
id: l-ecaea64f
scope: repo
status: active
src: agent
touches:
  - crates/aq-session-host/src/reader_lease.rs
  - crates/aq-session-host/tests/local_tmux.rs
  - build-budget.toml
  - crates/aq-worker/src/bin/aq-worker/service.rs
  - crates/aq-worker/src/native/launch/claude_docker.rs
  - crates/aq-worker/tests/native_cli/docker_local_executor.rs
  - docs/operations/install-service.md
  - packaging/service/aq-bootstrap.sh
  - packaging/service/aq-install.sh
  - packaging/service/prepare-worker-install.py
---

## Decided

One managed store per node under `<state_root>/observations/<node id>` (`aq_observation_store::node`), one writer thread, one gapless node sequence. The thread drives the transport owner (Prepare, Activate, boot advance on every worker start because the boot ID is per process), collects each registered transcript in turn and delivers one sealed packet at a time. Launch code calls `Observations::register(SourceSpec)`; dropping the handle finishes the source. The in-sandbox runner's per-launch store was deleted: that runner is another process and cannot share a single-writer store.

## Tried and ruled out

- Reusing the per-launch `Collector`: each store starts its own sequence at 1, which the server ledger refuses.
- `OverflowPolicy::GapAndHold` and `Store::begin_send`: the first drops bytes and holds the source when the server is slow, the second gives up after three sends. The node uses `Hold` (the transcript file is the buffer) and resends the sealed packet until its receipt arrives.

## Watch out

- A failing `ensure!` inside a store write transaction leaves the store "uncertain" until reopened. Decide every refusal before the write (see `node::admit`).
- Do not edit `engine_store.rs`, `reader.rs`, `sessions.rs` or the engine's parser files casually: their SHA-256 is the parser reference stamped on events, and the server's catalog pins the format parsers' digests.
- The store's gap records (spool loss, source change, boot fence) carry control parsers the server's catalog does not list, so the server refuses the batch that carries one, and the gapless sequence then cannot advance. Every worker restart with a live source queues a boot-fence gap.
- The transport contract allows 64 boot advances per node and one Prepare ever; a worker restart is a boot advance.
