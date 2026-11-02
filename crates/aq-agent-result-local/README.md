# Local agent result claim, version 1

Pure typed data for one attempt-bound file outbox. No networking, credential
issuer, execution/stop authority, policy evaluator or Graph completion exists in
this crate. The contract is independent of the frozen Graph agent-access API.

The actual worker allocates a directory outside the captured worktree and retains
its directory/FD identity. The selected runtime maps its root read-only and only
`submissions/` writable. Binding, presence, acknowledgment and owner lock are
worker-published control data; no Graph/node/service credential is installed.
The worker consumer compares its own retained scope and actual opaque current
execution owner, never accepting a reread agent-writable JSON binding as authority.
Existing same-UID worker trust limitations still apply; this is not hostile-agent
isolation or a new cryptographic authority boundary.

- `binding.json`: `Binding` with exact existing LaunchBinding, actual execution
  record digest, frozen expected-outcome ref, and original operation/report/nonce
  UUIDs. The worker durably allocates them before publishing the directory.
- `presence.json`: `Presence` with binding digest, kernel boot ID and original
  immutable execution hard ceiling in BOOTTIME ns. It is a refusal guard, never
  proof of a current grant. No local renewal or replacement IDs exist.
- `owner.lock`: held exclusively by the actual worker's live completion context,
  never created or acquired as ownership by the client. Missing lock, changed
  boot or expired original ceiling refuses a new publication. The actual worker
  still decides admission; publication may race expiry and is only pending.
- `submissions/submission.json`: one immutable canonical `Submission`, max8192
  bytes. Input is only `{schema_version:1,conclusion:"..."}`. The client adds the
  original binding; it cannot select stop/source/effect/review receipts or change
  the offered expected outcome. Conclusion follows existing ResultReport Label
  rules: one nonblank UTF8 line, max512 bytes. Invalid/oversize input is rejected,
  never truncated. Exact checks/results/limitations are narrative claims, not
  trusted check receipts. The first pilot uses HumanOwner/required_checks=[] only.
- `ack.json`: bounded `Acknowledgment` matching both IDs and both digests, written
  only after the actual native journal stage fsync. Recorded names only the exact
  B1 report-recording acknowledgment; it is not review, eligibility, source/effect
  release or Graph completion. The native owner verifies the real remote receipt
  before projecting this file. The client cannot derive authority from the file.

The CLI will expose only:

```sh
"$AQ_AGENT_CLIENT" --state-dir "$AQ_AGENT_RESULT_DIR" result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"
"$AQ_AGENT_CLIENT" --state-dir "$AQ_AGENT_RESULT_DIR" result status
```

Submit publishes at most the original local intent and reports pending, exit10.
After ambiguity inspect status and preserve original IDs/bytes. A changed payload
conflicts. No original intent is automatically replaced, evicted or superseded.
Read-only status after owner loss is historical/recovery data; disk never restores
live execution or new-submit authority.

The worker's separate native producer retains the claim, obtains real physical
stop and central source retention, freezes the existing ResultReport, and records
it through B1. It may carry honest pending-effect IDs; B2 independently inspects
actual owners/review/settlement. Final assistant text or natural process exit
without the explicit claim is not a successful AQ result.

All instruction bytes must be included in the centrally counted startup module.
Adapter mapping explicitly declares the pinned executable and both environment
values in the runtime recipe and tool shell policy; no arbitrary external folders
or uncounted prompt suffix is granted. Suspension/parking is not a dependency.
