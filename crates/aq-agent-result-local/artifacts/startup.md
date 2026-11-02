Complete the exact assigned task and acceptance criteria provided above. Read the repository's AGENTS.md and applicable local instructions before editing. Keep changes within the granted workspace and use the declared tools. Run focused checks that exercise the change; name their actual commands and outcomes. Record failed, unavailable or unrun checks honestly.

Finish by submitting one explicit AQ result claim. Write this bounded JSON request to "$AQ_AGENT_RESULT_DIR/submissions/request.json":

    {"schema_version":1,"conclusion":"Changes: ...; Checks: exact commands and outcomes; Limits: unmet items or limitations"}

The conclusion is one nonblank UTF-8 line, at most 512 bytes. Keep it factual and omit credentials. Then run:

    "$AQ_AGENT_RESULT_COMMAND" result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"

Submit waits up to two seconds, capped by the original execution deadline, for a matching worker acknowledgment. Exit code 10 alone means pending; inspect the returned phase. Accepted or finalizing means the worker retained the claim; recorded means report recording only. Stop editing after submission and end the turn after worker acceptance. If the wait times out with pending_local or reports uncertainty, preserve the original request, report the claim as unconfirmed, and never claim success or submit replacement bytes. Read its status with:

    "$AQ_AGENT_RESULT_COMMAND" result status

The result directory is a declared runtime channel outside the captured worktree. Its control files are read-only; only submissions/ is writable. The framework owns physical stop, source capture/retention and final result recording. Independent review and policy own task completion. Final assistant text and process exit alone do not submit an AQ result. Follow only the installed scoped interface; this protocol grants no task-close, Graph-completion or additional-folder authority.
