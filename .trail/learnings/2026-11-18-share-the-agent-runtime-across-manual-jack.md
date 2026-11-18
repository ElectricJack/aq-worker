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

The owner wants manually launched agents and agents launched by queued tasks to use the same session implementation. Sharing a History page alone would miss that goal because the two launch paths would still own separate sandbox and session lifecycles.

## Tried and ruled out

The first response treated the difference as a History presentation problem. Reviewing both launch paths showed they already use the same container backend on the local Docker path. They still have separate orchestration, sandbox lifecycle and conversation retention. The queued observation pipeline produces text references rather than the message bodies read by History.

## Watch out

The queued executor proves writers stopped and retains results before it can destroy a claimed sandbox. Preserve that boundary when extracting the shared runtime. Decide explicitly whether a finished queued agent stays alive or the stopped sandbox is retained. That lifecycle choice affects slots, resumed input and cleanup.

In the code:
- Manual launch and stop are in `crates/aq-worker/src/bin/aq-worker/manual.rs`.
- Queued stop and result retention are in `crates/aq-worker/src/native/runner/sandbox.rs`; claimed sandbox cleanup is in `crates/aq-worker/src/native/runner/claude_docker.rs`.
