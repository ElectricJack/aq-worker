---
title: "Node git: one repository per project, a worktree per attempt, in crate aq-node-git"
author: Jack
date: 2026-11-19
branch: feat/e0-w2-node-git
cost: { minutes: 1078, tokens: 2434858 }
id: l-08f4d96d
scope: repo
status: active
src: agent
---

## Decided
- Epic 0 W2/W3 git mechanics live in `crates/aq-node-git` (the aq-worker lib may not grow); aq-worker has only glue under feature `delivery_playbook`, selected at runtime by `NativeConfig.node_git` (absent: the bundle path, which is what every existing test still runs with `--all-features`).
- The grant is plain JSON from `GET /v1/git/grants/<attempt>` (aq-server #198), not an aq-admission type, so no vendored protocol change. Everything that depends on the server contract (grant route, `/v1/git/<repo>`, `Authorization: Bearer` node credential, `AQ-Attempt`, S6 `AQ-Pushed-Ref`/`AQ-Pushed-Head`) is in `transport.rs`. Headers reach git only as `GIT_CONFIG_COUNT`/`http.extraHeader` environment, never argv.
- The worktree lives under the slot's aq-git root (`<git root>/worktrees/<attempt>`) because result custody (`result/roots.rs`) requires the workspace inside that root; the repository itself is node-wide. A `<path>.grant.json` record beside the worktree lets salvage finish it after a restart.
- A worktree keeps its `.git` file; `collect_tree`/`sync_tree` already skip `.git` at the root, so the sandbox still gets only the tree.

## Watch out
- After a dropped receive-pack reply, curl may resend the POST itself; the push run fails and the next run (same lease) sees the ref "up to date" (porcelain `=`), which is success. A stale lease shows as `!` with `(stale info)`: that is `push_conflict`.
- `--force-with-lease=<ref>:` with an empty value means "the ref must not exist".
