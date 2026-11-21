---
title: Use the build’s own handle to decide when an AQ worker can finish
author: Jack
date: 2026-11-21
branch: fix/unfinished-results
cost: { minutes: 448, tokens: 2355771 }
id: l-1d451a3e
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/tests/driver.rs
  - crates/aq-handoff-note/src/claude-stop.sh
  - crates/aq-handoff-note/src/harness.rs
  - crates/aq-worker/src/native/runner/recovery/tests.rs
  - crates/aq-node-service/src/native_transport.rs
  - crates/aq-worker/src/native/http/phase_terminal.rs
  - crates/aq-handoff-note/src/stream.rs
  - Cargo.lock
  - crates/aq-codex-mapper/src/lib.rs
  - crates/aq-node-git/Cargo.toml
---

## Decided
Collect a build's exit status and log through the original tool task. A managed worker can then distinguish a completed check from an unfinished turn.
Use the node’s grant and the published merge parents to establish what the sandbox validated. Tell the agent when the node has already integrated main and will publish the branch after handoff.

## Tried and ruled out
Two independent shell monitors on a real attachment task kept matching their own command lines long after Cargo had exited with a compile error. The UI's terminal timestamp also predated the agent's latest progress. Checking the build log, process ancestry and observation feed identified the false wait. Stopping just the two read-only monitors woke the same agent, which fixed the compiler error on its retained branch.
A returned server task passed its tests but reported that the sandbox lacked Git metadata and could not update the branch. Its published head had exactly the retained task head and previously current main as parents. Main had advanced again during validation. Cloning in the sandbox would not have fixed that race.

## Watch out
Another attempt finished checks and then lost its stopped-report acknowledgement to database contention. Retrying that temporary refusal with the same request preserves the result. Recovering a missing acknowledgement and waking a build monitor address different failure modes.
In the code: crates/aq-handoff-note/src/harness.rs supplies RUNNER_NOTE. crates/aq-handoff-note/src/claude-stop.sh determines when a managed Claude turn yields its final report.
A nonempty conflict list requires real source edits. A clean integration at startup can still fall behind during tests. Protected delivery rechecks the published head.

In the code: aq-node-git prepares the worktree before sandbox_driver assembles the prompt. Server PR #222 head 8219eae0 is the observed clean merge.
