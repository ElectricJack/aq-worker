# Fresh implementation review

Read-only review by a fresh GPT-6 Astra agent under executing-plans. Reviewed
`303c5f3..2b349fe` plus the complete final working-tree implementation and tests
(`/tmp/b6a1-review.diff`). No Critical or Important findings. Assessment: ready
subject to required checks passing and final evidence being recorded.

Deferred Minor findings:

1. Successful renewal integration releases the fixture before the original hard
   deadline. A repeated-renewal run all the way to the hard cap would strengthen
   coverage. The implementation still caps every accepted renewal at the
   unchanged original hard deadline; B1 and host validation also bound authority.
2. Integration tests withhold receipts and unit tests reject malformed receipt
   scope/digests. Transport errors and receipt-journal persistence failures are
   not separately injected end to end. Those paths preserve state and do not
   invoke completion; additional injection would strengthen regression coverage.

Author's rulings on behaviors the reviewer set aside:

- HTTP/server compatibility remains deferred to B2/B6a2 under the explicit
  supervisor instruction. Cost if wrong: no production sandbox launch until
  authenticated endpoint wiring is supplied; the CLI already refuses clearly.
- Production source export and central receipt authentication belong to the
  trusted transport. The fixture writes only synthetic source-output bytes and
  exact stopped evidence. Cost if wrong: an incorrectly implemented future
  transport could falsely attest retention; these tests do not qualify it.
- Hostile shared-kernel isolation/provider qualification are outside B6a1.
  Cost if wrong: using the local trusted fixture boundary as a hostile-code
  sandbox or production provider would be unsupported.
- Full check results are owned by the parent executor and recorded separately;
  the reviewer did not claim them. Cost if wrong: publication would lack the
  mandatory verification gate; the task will not pass without that gate.
- The shared machine was not suspended. BOOTTIME reads, aged handoff rejection,
  stalled transport expiry and independent guardian enforcement are inspected
  and tested. Cost if wrong: an OS-specific suspend behavior would require
  additional environment qualification; no physical suspend test is claimed.

Post-review verification adjustment: the full suite exposed existing journal
fixture timing races (400-ms local observation caps across real HTTP/fsync).
The author moved preparation/refresh journal scenarios to the existing scoped
test clock, preserving explicit expiry and all history assertions; the observed
refresh physical scenario uses that clock too. Production code is unchanged.
This small test-only adjustment was author-checked, not a second reviewer round.
The same clock also stabilizes the observed interrupted-replacement write-fault
test, which otherwise expires before reaching its injected fault.
Cost of the test-clock adjustment: those journal unit scenarios no longer
qualify physical passage of time. The sandbox runner integration still reads
real kernel BOOTTIME and tests authority expiry with real guardian processes.
