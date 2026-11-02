# Shared finite Responses fixture data

Source `c0c6be5` extracts the existing Codex fixture SSE builders and bounded request checks for reuse by the composed execution fixture. Old wrapper values, event fields, IDs and commands are preserved; the independent source reviewer found no blocker. No production default feature exposes the module.

Three pure cases passed, covering complete-message/event framing, distinct tool IDs/escaped commands and wrong model/cap/policy/credential refusal. Affected test-support library/tests strict checks and default library compilation passed. Commands/timings and raw logs are retained alongside the qualification record. No actual Codex, model, listener, process, SQL or task settlement ran in this qualification. Usage values in these synthetic SSE frames are fixture data, not accounting evidence.
