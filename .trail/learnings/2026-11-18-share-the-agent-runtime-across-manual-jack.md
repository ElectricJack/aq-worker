---
title: Share the agent runtime across manual launches and queued tasks
author: Jack
date: 2026-11-18
branch: design/shared-agent-runtime
id: l-c0dbbe11
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/manual.rs
  - crates/aq-worker/src/native/runner/sandbox.rs
  - crates/aq-worker/src/native/runner/claude_docker.rs
---

## Decided

The owner withdrew the broader runtime unification. The requested change is to pull the provider session JSONL from queued attempts and use the same message processing and History reader as manual sessions. Keep the sandbox and task lifecycle policies because sharing history does not require changing either one.

## Tried and ruled out

The earlier design proposed sharing the entire runtime. A terminal capture does not replace the provider records because terminal output can distort text and omit message roles.

## Watch out

The queued Claude sandbox can be removed as soon as its result claim is recorded, so project and stage the provider messages before that point. Follow the updated learning “Use provider session logs for queued and manual agent history” on the implementation branch.

In the code:
- Manual capture starts in `crates/aq-worker/src/bin/aq-worker/manual/conversation.rs`.
- Queued Claude execution is in `crates/aq-worker/src/sandbox_driver.rs`.
