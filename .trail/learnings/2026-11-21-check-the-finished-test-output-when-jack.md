---
title: Check the finished test output when the polling shell stays alive
author: Jack
date: 2026-11-21
branch: integrate/held-worker-101
id: l-964f2db6
scope: repo
status: active
src: agent
touches:
  - crates/aq-handoff-note/src/claude-process-wait.sh
---

## Decided
Wait on the original task handle or read its saved exit status because a process-name search can keep finding its own shell after the child exits. If a live worker has an old Bash observation after the build has gone away, inspect the monitor as well as the build.

## Tried and ruled out
The runner already told agents to wait on the original task ID. A managed Claude session still started a full-command-line process search inside its polling shell. The search found the parent shell indefinitely. The test's retained output showed seven passes and exit zero. Interrupting just that shell let the same session start its next check.

## Watch out
A slow build and a self-matching monitor can look identical from session freshness. Verify the saved result and the live process identity before interrupting anything. Restrict the hook to the unbounded literal loop so PID waits, bounded checks and ancestor-excluding searches still work.

In the code: crates/aq-handoff-note/src/claude-process-wait.sh and its process-wait regression in harness.rs.
