# Codex interactive worker: ChatGPT login

This ports v1's existing Codex subscription login into the v2 scoped material and
private-home launch path. V1's `src/sessions/default_harnesses/codex.md` selects the
interactive TUI and prompt argument; `src/sessions/spec.py` constructs its argv.
The prepared selection uses the installed CLI's cached `gpt-6.1-sol` entry and
`low` reasoning.

`credential-profile.json` is the immutable public definition. Its exact reference
is in `credential-reference.json`. `server-credential.toml` adds the configured
owner-only auth source and permits only the newly enrolled visible worker node.
Root composes this credential with the joined interactive runtime/helper pins and
qualified harness configuration. The canonical configuration has not been edited.

Prepared copies are in
`/home/jkern/.local/state/agent-q/interactive-worker/provider/`, with directory mode
0700 and file mode 0600. These files contain configuration and public metadata;
the credential remains at `/home/jkern/.codex/auth.json`.

The daemon's existing material broker supplies that exact document under the
selected profile, Node placement and launch lease. The worker validates its
material/manifest binding and the host publishes it into a fresh private
`CODEX_HOME/auth.json`. Native Codex uses its ChatGPT provider endpoint and file
credential store. The startup prompt remains the exact declared prompt argument,
while stdin remains the PTY. The existing API-key execution path is unchanged.

Codex may refresh its private auth file. Cleanup accepts a valid owner-only
refreshed login file only after the existing matching ECHILD proof. This code
does not silently write refreshed tokens back to the operator's source. Future
launches reread the configured source; account rotation requires normal operator
credential revision/reload handling.

At preparation time the existing login file was owned by the daemon user, mode
0600, and its access token was unexpired by the local clock. No credential was
copied into an agent, no provider was contacted, and availability has not been
probed. The next step is one joined interactive-worker build and one bounded
real-provider session after the daemon and worker configuration are connected.
