---
title: "A docker-local attempt's transcript directory is bind-mounted into its driver directory and registered with the node store at launch"
author: Jack
date: 2026-11-19
branch: feat/transcript-mount-observations
cost: { minutes: 686, tokens: 1417594 }
id: l-f4d48fd0
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/native/launch/claude_docker.rs
  - crates/aq-worker/src/backend/docker_local/config.rs
  - crates/aq-worker/src/backend/docker_local/policy.rs
  - crates/aq-worker/src/sandbox_driver.rs
  - crates/aq-observation-redb-prototype/src/node.rs
---

## Decided

The launch (`native/launch/claude_docker.rs` bootstrap) creates `<host root>/driver-<launch>/transcript` (0700) and sets `DockerLocalConfig.transcript = (that directory, DriverHarness::transcript_dir())`. The backend mounts it read-write at exactly that target (Claude `/aq/home/projects/-aq-work`, Codex `/aq/home/codex/sessions`); `DockerLocalConfig::validate` admits only those two targets, and `create` only the node's own 0700 directory. Once the host is Running the launch registers `SourceSpec::of_launch` with the node's `Observations` and keeps the `SourceHandle` in `Physical.observation`, dropped beside the agent channel in `finish_stopped_transport`. The directory goes with the slot's runtime directory.

Claude's session ID is the attempt ID (`--session-id`, added by the driver whenever the mount or the conversation outbox is set). Codex chooses its rollout ID itself and nests the file under `sessions/YYYY/MM/DD`, so a Codex source registers with an empty `native_session_key` and the store thread (`node::codex_rollout`) finds the earliest rollout and takes the ID from its file name; the engine then checks it against `session_meta`.

## Tried and ruled out

- A polling task in the worker that waits for the rollout and then registers: it adds lines to the aq-worker lib (which may not grow) and a second reader of an untrusted directory. The store thread already polls and already has the `discovery_entries` limit.
- Carrying the mount in `CreateRequest`: the request hash is a label of the sandbox; the mount is node policy like `runner_command`, so it lives in the per-launch config.

## Watch out

- The mount is outside the sandbox's disk limit: the harness can fill the node's runtime directory (often tmpfs) through it.
- The container runs as its uid 0 (the template sets no USER), which rootless Podman maps to the worker's uid in both mapping modes, so the 0700 directory is writable inside and the files are the worker's outside (`owner_uid: None`). A template with a USER line breaks this.
- The end-of-attempt conversation capture still reads `<attempt>.jsonl` by copy-out, now through the bind mount.
