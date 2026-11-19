---
title: "Docker-local Codex writes a refreshed login back to the server's login store"
author: Jack
date: 2026-11-18
branch: feat/codex-login-write-back
cost: { minutes: 603, tokens: 1269836 }
id: l-a7a235f0
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/native/runner/claude_docker.rs
  - crates/aq-worker/src/native/materials.rs
---

## Decided
The server's login store (aq-server profiled_execution/login_refresh, C5b) is the one source of truth for a subscription login. A docker-local Codex sandbox gets auth.json writable (0600); before Stopped is reported the executor's pre_stop (runner/claude_docker.rs write_back_login) reads CODEX_HOME/auth.json from the stopped sandbox and, when it is the same account with a later last_refresh, posts it as version N+1 based on N under the node credential (aq-agent-channel login.rs NodeLink::write_back_login). N is the aq-material-version header, kept in the private material record (materials.rs Record.login_version). The public half of the lease is login-lease.json beside driver.json.

## Tried and ruled out
A node-local refresher with a file lock: it would be a second source of truth beside the server store. Reading the login back in the driver: the driver is killed by the host stop on cancel and deadline, so the executor reads it after it has stopped the sandbox itself.

## Watch out
The write-back route needs no runner permit: delegated_permits::runtime::authenticate_runner falls back to the node credential when no aqp_ token is presented. The server refuses a write-back once the attempt is cancelled (hot.revoked) or Stopped (status Finished), so a refresh in a cancelled attempt is lost. The driver's leak scan knows only the leased tokens, not ones Codex refreshed in the sandbox.
