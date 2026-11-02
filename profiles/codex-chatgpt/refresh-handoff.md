# First interactive session: preserve native login refresh

V1 did not implement a separate OAuth refresh client. Its Codex login descriptor
(`agent-queue2/src/install/logins.py`, `CODEX_LOGIN`) uses `auth.json` under the
selected `CODEX_HOME` or the user's `.codex` directory. Session environment and
Codex trust setup preserve that native file-store behavior. Native Codex updates
the shared login; AQ does not translate or restamp its token document.

V2 deliberately supplies an exact scoped copy in a private per-launch home.
The existing operator document has an unexpired access token, but its recorded
last refresh is old enough that the first real session may refresh. Neither
token rotation behavior nor refresh-token reuse safety has been proven from
local Codex source. Do not assume the original refresh token stays usable.

For the first bounded session, the launch owner must retain the original exact
delivered material and its source identity privately. Keep account IDs, tokens,
token hashes and source-file contents out of logs and public evidence. No extra
provider call or timestamp adjustment is needed.

After actual matching ECHILD, inspect the exact recorded private home before
credential cleanup. Unchanged auth requires no sync. A changed, valid owner-only
ChatGPT document must survive cleanup until the operator has resolved refresh
handoff. Preserve the existing launch/home identity checks and never follow a
replacement directory or symlink. Native process exit alone does not establish
ECHILD.

The operational handoff is:

1. Compare the refreshed document privately with the exact original delivered
   document. Require native ChatGPT mode, no API key, and the same nonempty
   `tokens.account_id`. Where JWT identity is readable, require unchanged
   identity issuer/subject too; do not treat unverified JWT claims as new
   authority. Reject malformed or unexpectedly different identity rather than
   importing another account.
2. Preserve the actual refreshed bytes, unchanged, in an operator-only recovery
   location outside the agent workspace (directory 0700, exclusive file 0600,
   no-follow, fsync). This is credential material, not an evidence artifact.
3. Before replacing the configured operator source, require exclusive operational
   ownership of its writers. An advisory AQ lock does not serialize independent
   Codex processes. Under that condition, reopen the source without following
   symlinks and compare its owner, mode, device/inode and exact bytes with the
   original captured source. If anything changed, refuse sync and retain the
   refreshed recovery copy; do not overwrite a newer login.
4. On successful original-file comparison, write the exact refreshed bytes to a
   new exclusive 0600 sibling, fsync, recheck the original source, atomically
   replace it, and fsync the directory. Do not merge JSON, fabricate a refresh
   time or print values. Without quiesced writers there is no strong filesystem
   compare-and-swap guarantee across that final check and rename: leave sync
   pending instead.
5. Re-read and validate the installed document privately. Use the existing
   credential revision/reload mechanism for subsequent leases; old material
   digests and launch evidence remain immutable. Only then resolve recovery
   custody and remove the stopped private auth copy.

This is a one-session operator procedure, not an automatic refresh broker. No
operator-source write is authorized merely by an account match or a refreshed
timestamp. At preparation time no credential was copied into an agent and no
provider or refresh endpoint was called.
