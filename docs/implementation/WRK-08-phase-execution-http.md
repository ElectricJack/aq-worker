# Phase execution HTTP observations

Source `21404a2` adds `NativeClient::prepare_execution` for the future
`/v1/worker/profiled-v2/prepare` endpoint. It uses frozen admission0.1.9
`Document::Prepare` and `ExecutionDelivery`, with the actual opaque proposal and
Accepted observation. No production daemon route is enabled.

First send must precede the actual accepted preparation deadline. A later retry
requires a previously received opaque execution observation and the same immutable
request/receipt; it can shorten but cannot extend execution or hard deadlines.
The first network reply and local timestamps survive retries before persistence.
The result is not serializable or convertible into a legacy Grant or host permit.
The driver must still join actual physical inputs and durably retain the received
phase execution history through a separately qualified owner.

All11 loopback phase tests passed in0.85seconds, including four new execution
cases: independent execution lifetime/replay, malformed or historical responses,
expired observation refusal, and canceled first reply with no late first-send
authority. Existing control-lane, byte-bound and nonce tests remain passing.
Strict library/test lint passed. [Exact source and logs](WRK-08-phase-execution-http.json)
retain the scope; this is not daemon, host, provider or full driver qualification.
