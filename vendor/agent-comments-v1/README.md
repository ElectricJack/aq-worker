# Agent comments v1

These are the agent-facing task comment routes served by `aq-server` (aq-dashboard spec 0004 §4.1). An agent leaves findings on its own task, the way v1's `aq task comment` works, and reads the task's thread, including comments left for earlier attempts. `routes.json` lists the routes, bodies and error codes; the unit test `agent_comment_routes_match_contract` checks the route table against it. The model is `crates/aq-task-comments`.

## Rules

- **Authentication:** the per-attempt machine credential of an active attempt. The project, task, session and attempt come from the authenticated binding, so an agent can only comment on and read its own task. A revoked or expired credential gets 401.
- **Add** (`POST /v1/agent/comments`): records a comment with `author.kind = "agent"` and the attempt's `session_id` and `attempt_id`. The comment ID is the `operation_id`.
- **Replay:** the same `operation_id` with the same body returns the same comment. A different body, or an `operation_id` already used by another author, gets `id_conflict`.
- **List** (`POST /v1/agent/comments/list`): the task's thread newest first, `limit` 1-100 (default 50). Pass `next_cursor` back as `cursor` for the next older page. The cursor is a position only and grants nothing. Deleted comments appear as tombstones with `body: null`.
- **Owner side:** owners read the thread through `GET /api/human/projects/{p}/tasks/{t}/comments` (human-graph-1) and write through the `dashboard-actions-1` kinds `task.comment.add|edit|delete`. Agents cannot edit or delete comments.
- **Startup context:** the thread is not yet part of the attempt's startup context. That context is the digest-pinned `ContextSnapshot` artifact assembled by `profiled_execution::producer::build::assemble` through `profiled_execution::context_projection::project`; the agent-access/v1 `ContextSnapshot` schema is closed (`additionalProperties: false`) and pinned by aq-worker, so adding the thread there is a coordinated agent-access contract change. Until then a later attempt reads the thread with `comments/list`.
