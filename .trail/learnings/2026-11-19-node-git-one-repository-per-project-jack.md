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
- Epic 0 W2/W3 git mechanics live in `crates/aq-node-git` (the aq-worker lib may not grow). aq-worker only forwards feature `delivery_playbook` to it until the vendored aq-admission carries the offer's `GitGrant` and the report's pushed ref (aq-server S5/S6).
- Everything that depends on the server's `/v1/git` contract (URL, `Authorization: Bearer` node credential, `aq-git-token` header) is in `transport.rs`. Headers reach git only as `GIT_CONFIG_COUNT`/`http.extraHeader` environment, never argv.
- A worktree keeps its `.git` file; `collect_tree`/`sync_tree` already skip `.git` at the root, so the sandbox still gets only the tree.

## Watch out
- After a dropped receive-pack reply, curl may resend the POST itself; the push run fails and the next run (same lease) sees the ref "up to date" (porcelain `=`), which is success. A stale lease shows as `!` with `(stale info)`: that is `push_conflict`.
- `--force-with-lease=<ref>:` with an empty value means "the ref must not exist".
