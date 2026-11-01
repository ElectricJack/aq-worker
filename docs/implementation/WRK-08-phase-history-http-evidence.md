# Bounded phase history HTTP client

Source `85ba8096d2419c207545ab6e807191bdbe2c1f3b` adds one read method,
one module export and loopback tests. No frozen contract/archive, Cargo/private
journal, opaque execution type, driver, host or smoke binary is changed.

`NativeClient::phase_terminal_history(&HistoryRequest)` posts actual known lookup
keys to `/v1/worker/profiled-v2/history` using protected node authentication and
exact version header1. It validates node scope, strict frozen request/receipt
correlation, exact single response version/JSON and64KiB body bounds. It uses
ordinary two-slot control capacity, with existing no-proxy/no-redirect/no-retry
transport. The return is plain `HistoryReceipt`, without clock sampling, execution
observation, durable adoption, host action, renewal or release. Snapshot absence
is not NeverGranted and may become known on a later identical lookup.

Six all-feature tests pass0.02s; five no-default-feature tests pass0.02s. Strict
all-target/all-feature worker Clippy passes13.16s and formatting passes. Tests
cover exact known keys, absent→known, missing/duplicate/wrong version headers,
wrong binding/accepted digest, extra/omitted-nullable fields, oversized bodies,
503 without automatic retry, and invalid/foreign requests before network. With
ordinary slots blocked, a Stopped request still reaches the absent test route and
returns404, proving independent reserve dispatch rather than a remote ACK.

[Evidence](evidence/wrk08-phase-history-http-85ba809/report.json) retains exact
source/archive hashes and raw logs. This is synthetic loopback qualification,
not a daemon history owner, kernel proof, full package rerun, recovered live
permission or runtime activation. Actual lost-first-execution-reply process
qualification is a separate subsequent opt-in smoke mode.
