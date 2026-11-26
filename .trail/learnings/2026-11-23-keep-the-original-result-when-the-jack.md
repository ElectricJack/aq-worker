---
title: Keep the original result when the harness exits before the worker acknowledges it
author: Jack
date: 2026-11-23
branch: fix/aq-checkpoint-recovery
id: l-bf9356e2
scope: repo
status: active
src: agent
touches:
  - crates/aq-agent-client/src/completion.rs
  - crates/aq-worker/src/native/result/history.rs
  - crates/aq-worker/src/native/result/mod.rs
  - crates/aq-worker/src/native/result/storage.rs
  - crates/aq-worker/src/native/result/tests.rs
  - crates/aq-worker/src/native/runner/recovery.rs
  - crates/aq-worker/src/native/runner/recovery/tests.rs
---

## Decided
Stopped recovery uses the original local submission if the sealed launch still owns its exact directory inodes and the submission has the original canonical bytes and binding. Recovery retains failed-outcome semantics and must prove stop and retain the source before reporting. An admitted or reported claim keeps its existing journal identity.
In the code: crates/aq-agent-client/src/completion/storage.rs shares the private local result reader between live intake and stopped recovery. The salvage_claim function in crates/aq-worker/src/native/runner/recovery.rs reads the original writer's pinned outbox and scans claim text before passing the retained submission to the existing salvage transaction.

## Tried and ruled out
The natural stop path can win before the live intake poll records the file, even when the harness submitted its final answer. Live attempt 0933e733 had the canonical submission in its outbox and a copied-back commit, but the generic recovery claim replaced its conclusion with a node-restart label. The worker PID had stayed up. Reinterpreting that file as a successful execution after stop would cross the live admission boundary.

## Watch out
Keep the stopped receipt and proper source copy-back. A local file or a posted task comment alone never proves success, and stale, altered or credential-bearing data cannot be used as the original result.
