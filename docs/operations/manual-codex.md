# Manual Codex runtime helpers

Manual sessions keep the installed sandbox template and native qualification
unchanged. If that template contains only the Codex CLI, configure the optional
`manual.codex.runtime_bundle` directory on that worker instance. It supplies
`codex-code-mode-host`, `codex-path/rg`, and `codex-resources/bwrap` from the
same Codex 0.166.0 package as the installed executable.

The launcher bounds and hashes each helper against the checked-in native-runtime
pins before sandbox creation. Nonsecret helper files enter through the sandbox's
created-only archive API. Startup verifies the installed CLI hash, copies it and
its helpers into the session's private home, and launches the complete runtime
there. Other sessions, templates, native permission/qualification inputs and
host binaries are unchanged. Provider authentication remains a separate private
ChatGPT subscription document; no host configuration directory or API key is
copied. Startup failure and explicit Stop remove its auth cache while retaining
project source.

Do not disable `features.code_mode_host` to compensate for a missing executable:
that disables the installed CLI's tool runtime. A text answer, including a model's
predicted command output, is insufficient validation. Verify a real tool-call
output in the CLI rollout as well as the browser terminal. This is manual-mode
acceptance, not a new finite-task/native resource qualification.
