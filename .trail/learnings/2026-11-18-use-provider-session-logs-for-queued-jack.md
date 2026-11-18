---
title: Use provider session logs for queued and manual agent history
author: Jack
date: 2026-11-18
branch: fix/queued-session-conversations
cost: { minutes: 4, tokens: 45659 }
id: l-de4d255b
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/sandbox_driver.rs
  - crates/aq-worker/src/bin/aq-worker/service.rs
---

## Decided

The owner narrowed the work to message history. Keep the existing sandbox lifecycle because persistence of the conversation is independent of persistence of the sandbox. The queued Claude driver enables the provider session file and projects its messages before claiming a result, because the claim can immediately trigger sandbox removal.

## Tried and ruled out

Terminal recordings replayed distorted screen output. Claude's print-mode event stream still serves the task result. Use the provider's session file for History because it records both the task prompt and the agent's messages. The earlier proposal to share the whole runtime was withdrawn.

## Watch out

The queued Claude recipe previously disabled session persistence. Its private config directory differs from the manual Claude default. Read the exact session by ID because Claude can leave additional sessions in the same directory. Save the projected messages outside the rotating slot because upload can outlast settlement or a worker restart. Only the original enrolled node can write history for a confirmed stop.

In the code:
- The Docker driver enables and reads the exact provider session in `crates/aq-worker/src/sandbox_driver.rs`.
- Both launch paths use `crates/aq-sandbox-archive/src/conversation.rs` to select user and assistant text.
- The service retries delivery in `crates/aq-worker/src/bin/aq-worker/service.rs`.
