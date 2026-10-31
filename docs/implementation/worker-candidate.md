# Standalone synthetic worker candidate, 2026-10-30

Environment: Linux 6.18.33.2-microsoft-standard-WSL2 x86_64; rustc
1.96.1 (31fca3adb 2026-06-26), cargo 1.96.1. Foundation sources were
copied from Agent Q `eed842362f29016ee508ca200f3bb3da2473dc08`;
foundation `aq-protocol` 0.1.0 source is unchanged. Worker 0.1.0 consumes
canonical `aq-worker-protocol` 0.1.1, execution version 2.

Acceptance covered locally:

| Work | Retained evidence |
| --- | --- |
| CON-01 candidate contract | Generated machine schema, semantic validators, executable valid/invalid vectors and ten conformance checks. Lost acceptance reduces TTL; changed operation payloads conflict; stale boot/generation, grant expiry, cancelled/stale retained results, nil/duplicate references and pending-operation release are covered. Both endpoints consume pinned public source packages; daemon/provider review lives in Agent Q. |
| WRK-01 connector extraction | Existing config/default path/identity preserved; 19 original connector tests plus foundation type test, two immutable fixture checks, and an independent fake-HTTP enrollment/save/reconnect/revoke lifecycle. No daemon or sibling build dependency. Product image qualification belongs to Agent Q. |
| WRK-02 journal/admission | One flock owner; strict versioned state; file fsync, rename and directory fsync before acknowledgement; poison on ambiguous directory-sync failure. Fourteen execution checks cover competing projects sharing capacity, replay, full-disk/write-failure injection, deadline/suspend accounting, stale grants, protected pending results and startup uncertainty. |
| WRK-03 synthetic process | Actual Rust child, one-shot durable launch claim and /proc start evidence, owned-Child reap before stopped proof, immutable durable report retry, release-before-retirement cleanup. Binary-to-fake-HTTP test discards one acceptance and one report response: exactly two identical requests each, one attempt/session/launch, accepted report, stopped writers and safe retirement. |
| WRK-04 partial control safety | Held synthetic child remains responsive to poll/cancellation/renewal; failed renewal through deadline kills and reaps it. Known stopped report recovers before new-boot registration without relaunch. These three binary process regressions pass; real descendant discovery/reattach and target-host suspend remain unqualified. |

Explicit bounds: node-wide slot/CPU/memory/disk reservations; preparation count;
pending result count/bytes; 64 pending observations per attempt; 64 directive
receipts per attempt; 1,024 retained attempt records; 32 MiB complete serialized
journal; 1 MiB HTTP response. Physical filesystem headroom is rechecked at
admission. A refusal never acknowledges an unpersisted action. Exact directive
replays remain valid at the receipt bound. Active/uncertain writers retain
reservations; stopped but unacknowledged reports retain storage. Local retired
history has a finite maintenance ceiling, not unbounded automatic growth.

Commands: `cargo test --workspace --all-features --locked`,
`cargo fmt --all -- --check`, and
`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
Targeted driver suite: four tests under one second excluding compilation;
execution suite: fourteen tests under one second; contract suite: ten tests.
The final component log is retained as `worker-candidate.tests.log`.

This is a sequential synthetic harness with one owned child and no spawned
process tree. Uncertain old writers are protected and require reconciliation;
there is no automatic relaunch or generic PID-based reattachment. Real harnesses,
tmux, scoped agent credentials/client/context, aq-git, source/artifact transfer,
central graph completion, long-load fairness and deployment/capacity qualification
remain later acceptance. Artifact handoff has wire metadata only; no source
retention is fabricated. No production deployment or remote publication occurred.
