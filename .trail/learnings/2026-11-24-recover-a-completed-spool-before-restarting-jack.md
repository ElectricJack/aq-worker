---
title: Recover a completed spool before restarting the slot
author: Jack
date: 2026-11-24
branch: fix/held-stopped-result-spool
cost: { minutes: 30, tokens: 399666 }
id: l-a767b45e
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/native/result/stopped.rs
  - crates/aq-worker/src/bin/aq-worker/service/slot_attempt.rs
---

## Decided

A private, canonical spool can exist without a journal claim because the run loop checks pane exit before polling the outbox. Check for the spool after intake is fenced and the stop receipt is verified. Then recover that same attempt through stopped-sandbox salvage while the slot's aq-git service is still running.

## Tried and ruled out

Writing a late claim into live result history risks capturing an older source tree because a failed driver can leave changes in the stopped sandbox's work volume. Salvage first verifies and copies back the stopped worktree. It then preserves the original report identity and exact submission bytes.

## Watch out

The typed pending submission changes when recovery runs. If it fails, the slot keeps its original journal for normal recovery. The spool is read under its original directory pins and writer lock.

In the code: DurableStoppedResult::record, NativeSlotBackend::run_slot_attempt, and salvage_interrupted.
