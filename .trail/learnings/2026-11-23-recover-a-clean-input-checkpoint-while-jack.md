---
title: Recover a clean input checkpoint while keeping the stopped sandbox quarantined
author: Jack
date: 2026-11-23
branch: fix/aq-checkpoint-recovery
cost: { minutes: 2675, tokens: 10430209 }
id: l-80b4c70c
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/service/recover_checkout.rs
  - crates/aq-worker/src/bin/aq-worker/service.rs
  - crates/aq-worker/src/native/runner/recovery.rs
  - crates/aq-worker/src/native/runner/recovery/tests.rs
  - crates/aq-worker/src/backend/docker_local/mod.rs
  - crates/aq-worker/src/sandbox_driver.rs
  - crates/aq-sandbox-archive/src/tar.rs
---

## Decided
After credential scanning refuses a stopped sandbox, a recovery may use a separate clean Git checkout only when the node verifies that it is the exact original checkpoint and still owns the attempt. This recovers from the trusted input without transferring anything from the stopped sandbox. Report the attempt as failed and use the ordinary result path.
When a slot has journaled Prepare and received no grant, read current authority for the precise retained worker, boot, attempt and launch. Recheck with backoff while closure is unresolved. Archive only after AQ proves it never granted that preparation.
Keep the original preparation journal on the normal recovery backoff when AQ may have granted it. A worker without a recorded launch or result archives only after it reads the exact never-granted closure or the server's exact release for a granted launch that never started. The server can produce the latter after grant expiry under its normal Started and renewal fences.
Retry a timed-out preparation inspection before writing to the original sandbox. If the reply to a start request is lost, query the same sandbox and accept a running state only after its ownership matches. Send one start request. Label preparation failures by the operation that was in progress.
Treat an exact, stopped, owned Podman work volume as a recoverable source even after its container disappeared. Reuse the worker's credential scan and stopped-result handoff. Confirm the sandbox identity and stopped state on both sides of the read.

## Tried and ruled out
Seven Code Health slots reported credential_in_workspace despite the retained Git worktrees being clean and equal to their admitted inputs. The ordinary stopped-sandbox salvage correctly refused to copy the untrusted contents. The older owner command only allowed sandbox_absent. Changing the slot marker or removing the sandbox to fit that path would bypass custody and could destroy unique data.
Four MS1 slots stayed held after their server claims had been closed as never granted. The old worker classified a missing reply as permanent and parked the slots without asking AQ. A local Prepare journal alone cannot prove whether the server granted it, and a transport refusal alone cannot replace the saved closure.
Treating an issued grant absent from the journal as a permanent operator hold stranded the local slot after the server released its task claim. A timeout, an unavailable historical row, a mismatched release, and stopped execution history do not prove that a missing launch never started.
The live reports grouped ten failures under the same backend timeout. The retained work volumes each held about 5,700 files, so a missing container did not mean that staging had done no work. The default Docker daemon had no records for this cohort. The dedicated rootless Podman API returned a create event and a remove event with neither an init nor a start for every exact container name. The historical reports still lack the name of the timed-out request.
The backend inventory correctly reported a stopped sandbox from the work volume labels, but the normal archive read required the container to exist. The owner's absent-sandbox command also refused the remaining volume by design. The native Podman export returned the tree without creating or starting a container. The live fixture recovered a root-level sentinel after the container removal. The retained production export had 5,715 files and matched the original pinned checkout.

## Watch out
Emitting a failed handoff and waking the existing service makes it consider the claim destroyable. Record the sandbox preservation marker before writing the handoff. Older running services already honor the lost-{attempt}.json path; keeping that path prevents a separate upgraded CLI from causing accidental destruction. Leave the actual sandbox stopped and preserve it for an explicit operator disposition.

In the code: service/recover_checkout.rs checks the held reason, node-owned stopped sandbox and input checkpoint; service.rs filters claimed destroys using the preserved-sandbox record.
In the code: reconcile_prepared uses the read-only Reconcile page with the original binding. AQ may refuse that historical read until it supports the exact phase-profile closure. A read that shows an issued grant needs its own no-start recovery and must preserve the slot and the journal. Do not reconstruct a runnable permit or stop evidence from it.
In the code: reconcile_prepared validates the response against the exact original binding and request. CurrentPhase::Released must name aq.granted-never-started/<attempt> at version 1 and have a nonzero grant revision. This is historical release evidence. Do not turn it into launch authority or discard the original journal.
An unacknowledged start can be in flight while a read still shows Created. Sending another start can run the agent twice if the first runner exits. An archive upload with a lost reply can also have side effects. Preserve the volume and the uncertainty when the backend cannot establish success.

In the code:
- crates/aq-worker/src/backend/docker_local/mod.rs owns the scoped inspection and start.
- crates/aq-worker/src/sandbox_driver.rs records the preparation operation in the failed handoff.
- Read Podman events from the worker's configured sandbox daemon. The main Docker daemon runs AQ infrastructure.
Container archives prefix every path with the directory name. Podman volume exports are relative to the volume root, so stripping one component would flatten nested paths and discard root files. Require the complete tar terminator for volume copy-back. Keep the same archive size, ownership, and credential checks.

- crates/aq-worker/src/backend/docker_local/mod.rs stopped_work_tree verifies the exact owned state and chooses the matching archive endpoint.
- crates/aq-sandbox-archive/src/tar.rs read_workspace_tree decodes the two archive layouts.
- crates/aq-worker/src/native/runner/recovery.rs salvage_interrupted uses the normal failed handoff with the retained tree.
