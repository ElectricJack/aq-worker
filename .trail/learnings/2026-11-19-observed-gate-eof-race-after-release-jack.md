---
title: observed gate EOF race after release
author: Jack
date: 2026-11-19
branch: fix/observed-gate-exec-race
cost: { minutes: 1102, tokens: 2597691 }
id: l-62c798ee
scope: repo
status: active
src: agent
touches:
  - "crates/aq-session-host/src/observed/gate.rs#released_read"
  - "crates/aq-session-host/src/local_ipc.rs#receive_or_eof"
  - crates/aq-session-host/src/supervisor.rs
---

About 3 in 85 sandboxed launches failed with the worker logging 'native host bootstrap refused: reader closed or malformed packet'. Cause: after release the guardian's observed gate polls the helper socket for hang-up, then reads it. The helper's exec closes its CLOEXEC socket; when that lands between the poll and the read, Socket::receive gets a zero-byte read, which it refuses as 'reader closed or malformed packet', so the gate failed closed although the driver had exec'd. Fix: released_read uses receive_or_eof and treats a zero-byte read with no descriptor, confirmed by hung_up() afterwards, as a hang-up; the next tick still needs live_pane plus observed_image exec evidence, and any packet (even an empty one from a live helper) still fails closed. The guardian's Err used to be discarded in supervisor.rs, so the stop reason never named the cause; it now carries {error:#}. Production guardian stderr is /dev/null, so the persisted stop reason is the only durable record. Deploying needs a new helper binary (aq.host/aq-session-host, host.helper digest in the node template), not just a new aq-worker.
