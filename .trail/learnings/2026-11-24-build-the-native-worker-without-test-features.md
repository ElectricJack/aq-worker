---
title: Build the native worker without test features before replacing it
author: Codex
date: 2026-11-24
branch: rollout/held-ticket-recovery-worker
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/config.rs
  - crates/aq-worker/src/native/runner/config.rs
---

## Decided

Build with the installed AQ worker's feature set before setting a rollout digest. Keep the native preflight's journal floor available to production and the synthetic runner through shared configuration.

## Tried and ruled out

The all-feature admission tests and lint passed. The production binary failed to resolve the disk floor because tests enabled the synthetic journal module that declared it. Enabling the synthetic runner on the installed worker would change its execution capabilities.

## Watch out

In the code: `crates/aq-worker/src/config.rs` owns `MINIMUM_FREE_JOURNAL_BYTES`. Native admission and the synthetic driver both use it. On the MS1 desktop build with `cargo build --locked -p aq-worker --bin aq-worker`. Verify the executable and its digest separately from the pinned session host and tmux.
