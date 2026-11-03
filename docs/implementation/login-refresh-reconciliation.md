# Desktop ChatGPT login reconciliation

Task `fresh-delta-13`. Owner custody decision: 2026-11-02. Supervisor scope
decision `msg-a395ebbc3a7646d3b011f3c62bb6818b` authorizes this local implementation
with a typed injected write-back seam and fake tests. The actual C5b server
binding is follow-up work after aq-platform task `calm-delta-18.8` publishes its
contract. No endpoint, lease extension or wire types are invented here.

## Implemented boundary

`aq_session_host::SessionHandle::reconcile_chatgpt_login` takes the exact stopped
guardian's ECHILD proof, a desktop-configured absolute Codex home directory and
one `LoginWriteBack` implementation. It verifies the sealed launch/private input
binding. The method must run on the desktop owner, which is the sole writer of
its `auth.json`; the owner path must never come from sandbox input.

Both directories must be owned by the effective uid with mode 0700. Directory
components and private files cannot be symlinks. Credential files must be regular,
single-link, mode 0600, bounded to 64 KiB, and valid native ChatGPT login JSON.
The desktop owner cannot be the disposable host root or private session home.
Bytes are zeroized after use and are absent from diagnostics and journal state.
Adapter errors are redacted because they may contain private response bodies.

The method acquires permanent `.aq-refresh.lock` in the host root and
`.aq-auth.lock` in the desktop Codex home, using nonblocking exclusive `flock`.
**Every desktop credential writer must cooperate on `.aq-auth.lock`.** The lock
is never unlinked. This excludes concurrent reconcilers, but cannot serialize a
CLI or operator that ignores the lock. Detected unexpected owner changes are
preserved as conflicts; no claim is made to prevent arbitrary same-uid writers.
The adapter must bound its network call because these locks remain held.

The local mode-0600 `chatgpt-refresh.json` is atomically replaced and fsynced,
including its parent directory, on every transition. It binds the launch,
original auth and installation-record digests, refreshed file inode/digest/size,
desktop directory identity, and a stable operation UUID. It records metadata
only, including the non-secret server revision reference.

| Durable phase | Meaning and retry behavior |
| --- | --- |
| `pending_owner` | Rotation and operation identity are pinned before external effects. The desktop login must match the original or exact already-published refreshed bytes. |
| `owner_updated` | Private staging was fsynced, renamed atomically over desktop `auth.json`, and the directory fsynced. Retry submits the same operation and bytes. |
| `acknowledged` | The adapter returned the exact operation/digest acknowledgement, with a durable server revision reference; the local acknowledgement is fsynced. Retry only cleans up. |
| `complete` | The refreshed worker file was unlinked and synced, its empty private home removed and synced, and completion durably recorded. |
| `conflict` | Desktop contents changed unexpectedly. Preserve both copies and require owner intervention; never automatically overwrite or roll back either copy. |

A crash after owner rename retries from the pinned operation without restoring
old credentials. A lost server response replays the same operation. A crash or
cleanup failure after durable acknowledgement never resubmits the write-back.
Missing auth after acknowledgement is recoverable; missing auth before it is
an error. Replacement/changed refreshed files are preserved. Unknown generated
files prevent removal of the private home; cleanup is never recursive.

`chatgpt_refresh_status` exposes the durable metadata for the same stopped proof.
Ordinary `cleanup_private_inputs` shares the reconciliation lock and refuses an
unfinished journal, including when the original credential bytes were unchanged.
Its pre-existing refusal of rotated credentials remains intact when no explicit
reconciliation has started. Immutable installed source material is never edited.

## C5b adapter and disposal handoff

`LoginWriteBack` is the only injected trait. `LoginRefreshRequest` and
`LoginRefreshAck` are opaque in-process types without serialization or private
Debug output. The request exposes the stable operation ID, launch binding,
original installation-record digest, original/refreshed auth digests and a
borrowed private byte slice. These are local facts, not a server protocol.

The future adapter must:

1. Resolve the original installation record to the correct C5b credential lease
   and profile, and authenticate under the server's actual contract.
2. Bind write-back to this operation, original revision, launch/lease and exact
   bytes; use the operation ID for immutable replay and perform server revision
   CAS and refresh-lineage fencing. A stopped proof does not renew an expired
   lease. Expired/revoked lease recovery needs explicit server authority.
3. Validate the server acknowledgement against the submitted identity and bytes.
   Only then call `request.acknowledge(non_secret_revision_reference)` and return
   it. HTTP acceptance alone is insufficient. The host checks operation/digest
   consistency, but trusts this adapter to establish server durability.
4. Return an error on rejection, ambiguity or timeout. Local rotated bytes remain
   retained, even if the desktop update has already succeeded.

The desktop/controller caller must persist its dependency on the host root and
preserve container/state on every error or phase other than `complete`. Complete
satisfies only credential cleanup: source, artifact and operator retention gates
still apply. No production server adapter or automatic backend-disposal wiring is
installed by this task. Existing production cleanup continues to preserve
rotated credentials. Integrating C5b/C6b and the controller disposal gate is the
explicit supervisor-authorized follow-up, not a claimed live qualification.

## Verification

Only synthetic auth documents and fake acknowledgements were used. No real
desktop credentials, provider refreshes, server vault revisions or containers
were accessed. Public host API tests use synthetic sealed stopped proof fixtures;
they do not claim to rerun live guardian/process qualification.

Run the committed wrapper through AQ's resource semaphore (Cargo build jobs and
Rust test threads are both capped at 2):

```sh
PYTHONDONTWRITEBYTECODE=1 LOGIN_REFRESH_CHECK=area aq test docs/implementation/login-refresh-evidence/test_cargo_check.py -q
PYTHONDONTWRITEBYTECODE=1 LOGIN_REFRESH_CHECK=clippy aq test docs/implementation/login-refresh-evidence/test_cargo_check.py -q
```

The wrapper records the exact Cargo command and complete output in
`login-refresh-evidence/area.log` and `clippy.log`. The `focused` selection runs
only `private_inputs::reconciliation` tests. Coverage includes owner conflicts
before and during exchange, lock contention, lost acknowledgement, interrupted
owner publication, acknowledgement/cleanup restart boundaries, actual unlink
failure, unknown home files, hostile file types/modes, foreign acknowledgements,
changed journal bindings, retargeted owner directories, exact proof requirements,
and preventing ordinary cleanup from bypassing a pending transaction.

Final validation after merging `origin/main` (`26edf5c`):

- `cargo test --locked -p aq-session-host --lib`: **53 passed, 0 failed,
  3 existing ignored**, including 17 reconciliation tests.
- `cargo clippy --locked -p aq-session-host --lib --tests -- -D warnings`: passed.
- `rustfmt --edition 2024 --check --config skip_children=true` on the four changed
  Rust files: passed. `git diff --check`: passed.

The existing ignored namespace/resource tests were not enabled; the scope is
the changed host library, not full workspace or live-provider qualification.
