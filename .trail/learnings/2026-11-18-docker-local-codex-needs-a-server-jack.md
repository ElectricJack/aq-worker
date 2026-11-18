---
title: Docker-local Codex needs a server-side admission and selection change before an offer can reach it
author: Jack
date: 2026-11-18
branch: aq/e0-w1
cost: { minutes: 156, tokens: 421565 }
id: l-2adb89ab
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/native/launch/claude_docker.rs
  - crates/aq-codex-mapper/src/docker_local.rs
  - vendor/aq-admission-0.1.13/src/preparation_shared.rs
---

## Decided

Codex in the docker-local sandbox is a second harness of the one docker-local executor (`settings.codex` in `crates/aq-worker/src/native/runner/claude_docker.rs`), not a second executor, because a node has one executor and the host profile, driver and template are shared. It has its own result adapter id `aq.codex-docker-result-local-adapter` and profile `codex-0.166-docker-local-result-local-v1`, because the existing id `aq.codex-result-local-adapter` means Codex's own sandbox. A Claude-only configuration keeps its bytes: `DriverConfig.harness` is not serialized for Claude and the host profile digest changes only when `codex` is set.

## Watch out

The worker side alone cannot run a Codex offer. `PhasePolicy::validate_for_manifest` in `vendor/aq-admission-0.1.13/src/preparation_shared.rs` admits a long (schema 3) phase only for interactive Codex with `aq.codex-result-local-adapter` or non-interactive Claude with `aq.claude-result-local-adapter`, so a non-interactive Codex manifest is refused on both sides until aq-admission gets a third case and the worker vendors that version. aq-server also names the Claude adapter in `aq-selection` (`NODE_SANDBOX_ADAPTER`) and accepts only known profiles and recipe digests in `profiled_execution/producer/result_startup.rs`.

The login file is 0400 in the attempt's own `CODEX_HOME` (`CODEX_RUNNER` in `crates/aq-codex-mapper/src/docker_local.rs`). That stops a refreshed login being written, not the refresh call itself: a login older than Codex's refresh interval would still be rotated at the provider from inside a sandbox, so the node must be given a fresh login.
