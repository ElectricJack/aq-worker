# Native Prepare join qualification

Source `82d3c07` passes **146 worker package tests**, with four explicitly declared actual-Git ignores, and **one separately enabled pinned actual-Git case**. Strict all-target/all-feature Clippy, formatting and source diff checks passed. An independent read-only review found no blocking issue. Exact logs and source hashes are retained in the adjacent JSON.

The real Git case now joins actual downloaded source, imported retention, a clean checked-out workspace, real private material installation and verified inline inputs into a durable Prepare request. HTTP coordinator/runtime/qualification claims remain synthetic. No host is invoked and this component check does not send Prepare to the real daemon or receive a live grant.

Focused cases prove stable request/operation/checkpoint on retry with newly verified inputs, exact original input set, renderer pin, current retention/head and installed-byte refusal, and four journal fsync cuts. Reopen retains either the original snapshot or the complete intent, preserves capacity and refuses live admission. Format4 refuses formats1/2/3 without rewriting them. The earlier142-test hydration count is historical and overlaps this suite.

Development evidence includes two fixture compile mistakes, an overly broad comparison of the three-field typed workspace to the full Git record (corrected by typed comparison while the existing full provenance guard remains), and a retry fixture that correctly hit the exclusive source owner lock until the first PreparedInputs was dropped. No deadline or authority check was relaxed.

Current source/material checks are observations, not permanent availability guarantees. The join caps inline bytes at8MiB and still obeys the8MiB encoded journal bound. Default Git admission needs21GiB+1MiB of logical reservation; physical capacity, cold preparation, production Poll/Reserve, host launch, grant persistence and restart recovery remain separate gates. No production capability is enabled.
