---
title: Retry an explicit busy response before tearing down the starting worker
author: Jack
date: 2026-11-21
branch: fix/unfinished-results
id: l-8cc792a5
scope: repo
status: active
src: agent
touches:
  - crates/aq-node-service/src/native_transport.rs
  - crates/aq-worker/src/native/http/phase_terminal.rs
---

## Decided
Retry a start report that receives the server’s explicit HTTP 503 row-lock refusal using the identical durable request, with the existing attempt bound and the caller's shutdown and execution deadline. A single database row lock should not tear down a worker as its harness starts.

## Tried and ruled out
Retrying stop reports alone did not cover this transition. In the observed incident the start response asked the node to retry. The node closed the agent channel, its initial stop report was refused, and recovery found the sandbox absent with no result. Only the prepared Git merge remained.

## Watch out
In the code: crates/aq-worker/src/native/http/phase_terminal.rs uses distinct control and safety lanes. A generic 503 lacks the row-lock guarantee. Preserve both lanes and the unknown disposition of lost transport responses.
