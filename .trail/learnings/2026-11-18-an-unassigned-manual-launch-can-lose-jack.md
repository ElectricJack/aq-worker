---
title: An unassigned manual launch can lose its pause before queued pollers yield
author: Jack
date: 2026-11-18
branch: main
id: l-bdf9d9eb
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/service.rs
---

## Decided
When a slot-aware worker leaves a launch at Waiting for a slot, distinguish a launch pending on the server from a job already handed to the worker. The pause must survive while the server still has an unassigned manual launch. A short pause pulse can end before the queued-work pollers yield, leaving local permits effectively full despite idle server slots.

## Tried and ruled out
The November 17 follow-up found Worker 2 still polling with four idle slots, four project slots, an enabled project, and no compute-holding attempt or active manual job on that node. Its launch remained pending with no worker observation and no assigned slot. The live poll's busy field was not retained.

## Watch out
The server's pending_count includes all desired-running sessions, even running ones. Using it alone for a persistent pause would also suppress queued work alongside a running manual session. An explicit count of unassigned launches can distinguish the cases.

In the code:
manual.rs asserts pause from pending_count, then replaces it with a predicate over local agents before the next poll. The server's worker.rs skips pending jobs entirely when busy is true, so the worker has no local agent from which to derive the pause. service.rs takes a queued permit before entering the poll loop.
