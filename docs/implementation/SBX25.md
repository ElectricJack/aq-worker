# SBX25 worker inventory disk measurements

Task: `calm-lantern-25`. Reapplies reference commit `395f32d` from
`aq/smart-torrent-92` onto current main source base `75eccd0`.

`controller::Inventory` serializes optional filesystem total/used/free bytes,
reserved disk bytes, preserved count/bytes, retained snapshot bytes, and saved
snapshots over allowance. Missing measurements are omitted and older reports
without these fields deserialize. `available_disk_bytes` continues to use the
configured capacity and reservations for admission.

`inventory()` supplies reservation totals and the stopped, undestroyed attempt
count. `measured_inventory()` adds backend measurements. Current main already
uses `PlacementBackend::disk_usage(Uuid)` for SBX20 running-sandbox watchdogs;
this implementation preserves that API and adds `measured_disk_usage` for SBX25
reporting. The fake backend exposes separate setters for the two kinds of usage.
The published `transport::Poll` and compatibility hashes are unchanged.

Docker-local measures the daemon storage root through an injectable statvfs
port. Used bytes derive from allocated blocks; free bytes use space available
to unprivileged writers. Preserved bytes sum container `SizeRw` and work-volume
`UsageData.Size`. Missing/negative sizes and failed calls leave the sum absent;
a confirmed volume-only sandbox can still be sized. Filesystem and preserved
measurements fail independently. An empty preserved set has a known zero size.
Boat helpers parse bare/enveloped snapshot listings and sum content bytes and
per-sandbox counts beyond the supplied allowance, without network calls.

Verification uses the existing `docs/implementation/B5a-evidence/test_cargo.py`
adapter under `aq test`, with Cargo jobs and Rust test threads limited to two.
The checks use fake statvfs, in-memory backend sizes and scripted Docker calls;
no live daemon, network connection or real sleep is part of these test filters.
Exact commands and results are recorded in the adjacent evidence directory.

The supervisor supplied the approved amendment text in message
`msg-a8383d531bff49b68c46286b62dba6bc` after the cross-project spec read was
refused, and approved the additive method name. Node pressure refusal thresholds
and central filesystem/Postgres reporting belong to aq-server and are outside
this worker change.

Checks passed (20 Rust tests total):

| Cargo command | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked --offline -p aq-worker --lib backend::disk::tests` | 2 passed | `SBX25-evidence/disk.log` |
| `cargo test --locked --offline -p aq-worker --lib -- controller::lifecycle_tests inventory_serialization disk_watchdog_stops_and_preserves_only_the_attempt_over_its_limit` | 12 passed | `SBX25-evidence/controller.log` |
| `cargo test --locked --offline -p aq-worker --test docker_local_backend disk_` | 6 passed | `SBX25-evidence/docker.log` |

Each command used `B5A_COMMAND='<Cargo command>'`,
`B5A_LOG=/tmp/calm-lantern-25-evidence/<name>.log`,
`B5A_RESULT=/tmp/calm-lantern-25-evidence/<name>.json`, and
`RUST_TEST_THREADS=2 aq test docs/implementation/B5a-evidence/test_cargo.py -q`.
The wrapper sets `CARGO_BUILD_JOBS=2`; its normal worker cap and marker defaults
were preserved. Logs and command/exit receipts were copied here for retention.
`rustfmt --check --edition 2024` on changed Rust files and `git diff --check`
also passed. `git diff --exit-code HEAD -- protocol/aq-admission` confirmed the
frozen protocol subtree was unchanged before committing.
