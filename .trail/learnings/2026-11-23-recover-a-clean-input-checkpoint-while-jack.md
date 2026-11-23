---
title: Recover a clean input checkpoint while keeping the stopped sandbox quarantined
author: Jack
date: 2026-11-23
branch: fix/aq-checkpoint-recovery
cost: { minutes: 890, tokens: 3489936 }
id: l-80b4c70c
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/service/recover_checkout.rs
  - crates/aq-worker/src/bin/aq-worker/service.rs
---

## Decided

After credential scanning refuses a stopped sandbox, a recovery may use a separate clean Git checkout only when the node verifies that it is the exact original checkpoint and still owns the attempt. This recovers from the trusted input without transferring anything from the stopped sandbox. Report the attempt as failed and use the ordinary result path.

## Tried and ruled out

Seven Code Health slots reported credential_in_workspace despite the retained Git worktrees being clean and equal to their admitted inputs. The ordinary stopped-sandbox salvage correctly refused to copy the untrusted contents. The older owner command only allowed sandbox_absent. Changing the slot marker or removing the sandbox to fit that path would bypass custody and could destroy unique data.

## Watch out

Emitting a failed handoff and waking the existing service makes it consider the claim destroyable. Record the sandbox preservation marker before writing the handoff. Older running services already honor the lost-{attempt}.json path; keeping that path prevents a separate upgraded CLI from causing accidental destruction. Leave the actual sandbox stopped and preserve it for an explicit operator disposition.

In the code: service/recover_checkout.rs checks the held reason, node-owned stopped sandbox and input checkpoint; service.rs filters claimed destroys using the preserved-sandbox record.
