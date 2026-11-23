---
title: Recover a clean input checkpoint while keeping the stopped sandbox quarantined
author: Jack
date: 2026-11-23
branch: fix/aq-checkpoint-recovery
cost: { minutes: 915, tokens: 3501941 }
id: l-80b4c70c
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/service/recover_checkout.rs
  - crates/aq-worker/src/bin/aq-worker/service.rs
  - crates/aq-worker/src/native/runner/recovery.rs
---

## Decided
After credential scanning refuses a stopped sandbox, a recovery may use a separate clean Git checkout only when the node verifies that it is the exact original checkpoint and still owns the attempt. This recovers from the trusted input without transferring anything from the stopped sandbox. Report the attempt as failed and use the ordinary result path.
When a slot has journaled Prepare and received no grant, read current authority for the precise retained worker, boot, attempt and launch. Recheck with backoff while closure is unresolved. Archive only after AQ proves it never granted that preparation.

## Tried and ruled out
Seven Code Health slots reported credential_in_workspace despite the retained Git worktrees being clean and equal to their admitted inputs. The ordinary stopped-sandbox salvage correctly refused to copy the untrusted contents. The older owner command only allowed sandbox_absent. Changing the slot marker or removing the sandbox to fit that path would bypass custody and could destroy unique data.
Four MS1 slots stayed held after their server claims had been closed as never granted. The old worker classified a missing reply as permanent and parked the slots without asking AQ. A local Prepare journal alone cannot prove whether the server granted it, and a transport refusal alone cannot replace the saved closure.

## Watch out
Emitting a failed handoff and waking the existing service makes it consider the claim destroyable. Record the sandbox preservation marker before writing the handoff. Older running services already honor the lost-{attempt}.json path; keeping that path prevents a separate upgraded CLI from causing accidental destruction. Leave the actual sandbox stopped and preserve it for an explicit operator disposition.

In the code: service/recover_checkout.rs checks the held reason, node-owned stopped sandbox and input checkpoint; service.rs filters claimed destroys using the preserved-sandbox record.
In the code: reconcile_prepared uses the read-only Reconcile page with the original binding. AQ may refuse that historical read until it supports the exact phase-profile closure. A read that shows an issued grant needs its own no-start recovery and must preserve the slot and the journal. Do not reconstruct a runnable permit or stop evidence from it.
