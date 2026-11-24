---
title: Preserve the first observed gate failure in the stopped proof
author: Jack
date: 2026-11-24
branch: fix/held-launch-evidence
cost: { minutes: 24, tokens: 553550 }
id: l-e9d164e2
scope: repo
status: active
src: agent
touches:
  - crates/aq-session-host/src/observed/gate.rs
  - crates/aq-session-host/src/supervisor/lifecycle.rs
---

## Decided
Keep the first observed gate failure through guardian reaping. A permit can expire during cleanup, so the later expiry would obscure the failure that triggered the stop. Give the baseline digest and the drained reader lease their own errors before writing the exec claim.

## Tried and ruled out
For two pre-Started closures, the archived native journals reproduced the immutable stopped observation, including the closed pane and combined digest/lease error. The runtime host directories and result spools held no separate gate-release evidence or result claim. The closure confirms the guardian reaped its children. The diagnostic alone cannot establish that the original launch was suppressed.

## Watch out
The first cause stays useful for diagnosis even when ECHILD arrives after the deadline. Reconciliation still needs independently qualified original-launch evidence or an authorized retained result.

In the code: `crates/aq-session-host/src/observed/gate.rs` checks the durable blank baseline and reader lease in `Owner::release`. `crates/aq-session-host/src/supervisor/lifecycle.rs` carries that reason through to the stopped observation in `Guardian::check_stop`.
