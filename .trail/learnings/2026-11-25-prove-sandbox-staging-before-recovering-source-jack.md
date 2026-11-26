---
title: Prove sandbox staging before recovering source
author: Jack
date: 2026-11-25
branch: fix/source-only-staging
id: l-6ed5c7eb
scope: repo
status: active
src: agent
touches:
  - "crates/aq-worker/src/native/runner/recovery.rs#salvage_claim"
  - "crates/aq-worker/src/sandbox_driver/drive.rs#stage"
  - "crates/aq-worker/src/backend/docker_local/mod.rs#stopped_work_tree"
---

## Decided

A stopped source-only sandbox needs node-private proof of complete input staging and runner start for its exact daemon container instance before its primary or writable sibling archives can replace checkouts. A confirmed prepare failure can retain the original failed submission without importing sandbox source. Ambiguous and legacy cases stay held.

## Tried and ruled out

The preserved failure in the server report did not prove recovery left source alone. The driver had persisted the original submission on disk ahead of the native journal. Recovery imported source before reading that submission, then reused the exact failure text.

## Watch out

A pre-stage preserved work volume can look exactly like a valid empty workspace after a runner deletes every file. Container ownership, a stopped writer, and a readable archive do not prove staging completed.

In the code: `sandbox_driver/drive.rs#stage`, `native/runner/recovery.rs#salvage_claim`, and `backend/docker_local/mod.rs#stopped_work_tree`.
