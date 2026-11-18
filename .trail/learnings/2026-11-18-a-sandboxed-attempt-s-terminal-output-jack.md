---
title: "A sandboxed attempt's terminal output stops for good after a slow guardian tick"
author: Jack
date: 2026-11-18
branch: fix/terminal-reader-continuity
cost: { minutes: 60, tokens: 26153 }
id: l-1d7b518b
scope: repo
status: active
src: agent
touches:
  - crates/aq-session-host/src/reader_lease.rs
---

## Watch out
The guardian's reader lease (crates/aq-session-host/src/reader_lease.rs, Owner::try_tick) read CLOCK_BOOTTIME once at the top of the tick and then received the client's Drained receipt. A receipt stamped after that read but before the receive failed drain_deadline's next.1 <= now check ("reader drain receipt stale"). The control reader is one-shot, so the guardian killed it and the attempt's output tail went Discontinuous: last_output_at froze while the agent kept working. On 2026-11-18 it hit most desktop attempts longer than a few minutes (load average 17 to 35); short attempts rarely lived long enough to hit it.

## Decided
Judge a receipt by a clock read after the receive. Every other lease check stays as strict as before.

## Tried and ruled out
It was not the worker rollback to 2e45e932, not tmux pause-after=1, and not permit renewal. A soak of OutputObserver against a real host failed at 129 s, 137 s and 204 s with now=73433799 next=(536, 73433825).

## In the code
The failure is silent in the worker: output_relay.rs drain() swallows the observer error and the publisher only logs discontinuous=Some(true). Test: local_tmux.rs drain_receipt_stamped_during_a_slow_guardian_tick_keeps_the_reader (ignored; run with --ignored). The fix is in the aq-session-host helper binary, which is installed and pinned separately from aq-worker (native.json host.helper), so a rollout must replace that file and its pin.
