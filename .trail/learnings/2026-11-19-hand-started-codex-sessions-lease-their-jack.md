---
title: Hand-started Codex sessions lease their login and keep it in step with the server
author: Jack
date: 2026-11-19
branch: feat/session-codex-login
cost: { minutes: 617, tokens: 1157837 }
id: l-5b7f3202
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/bin/aq-worker/manual.rs
  - crates/aq-worker/src/bin/aq-worker/manual/provider.rs
  - crates/aq-node-service/src/link.rs
  - crates/aq-node-service/src/config.rs
---

## Decided
A hand-started Codex session no longer reads a login file on the node ([manual.codex] auth_file is removed: a service.toml that still names it is refused, so the rollout removes the key). manual.rs create() leases the newest stored login with ServerLink::session_login before any sandbox exists; a refusal (LoginRefused) becomes the session's stage, e.g. the server's codex_login_stale reason. The session keeps a LoginLease (version, account fingerprint, last_refresh; no token) in state.json. sync_login runs every [manual.codex] login_check_secs while running: a same-account, later-dated auth.json in the sandbox is written back with session_login_refresh (compare-and-swap on base_version); when superseded, or when the server's version is newer than ours, the newest stored login is installed over the sandbox's auth.json by rename. finish_login does the last write-back and removes the login before the sandbox stops (owner stop, agent exit, worker shutdown). The age limit is the owner's Setting checked by the server at lease; max_login_age_secs is gone from the native template.

## Why installing into a running sandbox works
Codex 0.166 (codex-rs login/src/auth/manager.rs) reloads auth.json from disk before a proactive refresh and as the first step of 401 recovery, adopting a changed login when the account id matches; a cached permanent refresh failure is keyed on the exact credentials that failed and is cleared when the file content changes. It writes auth.json in place (truncate+write) under auth.lock and sets last_refresh to now.

## Tried and ruled out
Leaving the EXIT trap that deleted auth.json in agent.sh: a refresh in the last interval before the agent exited by itself was lost, which strands the stored login. The runner now keeps the sandbox up after the agent exits until the worker has collected and removed the login (its lease watchdog bounds the wait and removes the file on TERM). Scrubbing through put_archive: the docker-local backend only accepts archives in Created state.

## Watch out
The worker installs a newer login under Codex's own lock (flock on auth.lock, as storage.rs lock_for_write does); the sandbox template must have flock (util-linux). The python receiver (worker-install-config.py apply_logins) now accepts only the Claude token.
