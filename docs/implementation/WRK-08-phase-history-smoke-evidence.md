# Lost-execution-reply history-only smoke candidate

Source `ed21fbe2825142535f74dead475a426fcde8aa46`, using the separately qualified
[history client](WRK-08-phase-history-http-evidence.md), adds only opt-in test-support
smoke behavior. `phase_preparation.history_after_lost_execution=true` requires
actual input preparation and execution request, and forbids `synthetic_host`
before any I/O. Only an explicit503 execution response enters the history path;
success, other statuses or transport errors refuse instead of being swallowed.

The lookup uses the actual accepted digest, Prepare operation and exact binding.
It requires known historical identity, compares the actual Prepare digest,
guardian/launch/reservation and original runtime/qualification, then emits
`phase_history_only` with request/receipt/revision. The actual exclusive Prepared
owner remains alive through emit. No execution delivery is fabricated, no local
execution owner is recorded, no host/renewal/cleanup is called, and journal
revision plus absent execution/host records are checked before output. This is
historical data, not recovered authority or NeverGranted proof.

Four binary tests pass0.01s, including mode guards before I/O and503-only behavior.
Strict all-target/all-feature Clippy passes0.48s and formatting passes. Release
build takes68s. A test-only include path compile failure is retained, corrected
before freeze. No full package or actual daemon process result is claimed here;
root owns the separate controlled server-withheld-reply process qualification.

Pinned binary `/tmp/aq-phase-history-worker-pin/native-prepare-smoke`, mode0500,
is11,004,272bytes, SHA256
`673cde774bc7e03a563a7503cfa67a2812d967e8bea0b88b5b5b2cd8da60f8a3`.
The format10 physical and old format9 pins remain unchanged.
[Evidence](evidence/wrk08-phase-history-smoke-ed21fbe/report.json) retains source and
binary hashes, build/test/lint logs and explicit limits. No provider, default
runtime capability, renewal, task success, resource settlement or release.
