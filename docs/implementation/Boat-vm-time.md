# Boat VM-time reporting

Task: `fair-journey-58`. Server contract: aq-server
`origin/aq/grand-vault-92` at `da9ea1de5153b170e946cba765af8a39850fff7d`,
`docs/implementation/H2-monthly-boat-budget.md` and
`crates/aq-server/src/observations/api.rs`. Deploy the server endpoint before
this worker and complete historical backfill before enabling its monthly ceiling.
The server PR must merge before this worker PR.

The Boat backend posts to `POST /v1/worker/observations/vm-time` with the enrolled
node bearer and existing Cloudflare Access headers. The Boat API key stays on
the provider channel; runner permits never report these facts. The body is:

```json
{
  "attempt_id": "11111111-1111-4111-8111-111111111111",
  "sandbox_id": "bx_example",
  "size": "small",
  "started_at_ms": 1793491200000,
  "stopped_at_ms": null
}
```

`size` is `small`, `default`, `large` or `xlarge`; the existing creation policy
selects at most large. Creation time comes from Boat's RFC3339 `createdAt`,
including paid provisioning and preparation before the runner starts. See the
[Boat get response](https://docs.boat.dev/api/reference/sandboxes/get-sandbox).
Start and size never change on replay. A stop is filled once at the first
backend observation confirming archived state, snapshot availability and completed
stop status when supplied. Stop intent, HTTP 202, hard deadline, expiry and a
missing VM do not close an interval. When the controller was down, the first
confirmation on recovery is a conservative end; it does not infer an earlier
end from a TTL or `updatedAt`. Existing stopped facts keep their original end.

The existing locked Boat journal gains a default-empty `vm_time` map. Each fact
and its pending flag are atomically replaced and fsynced before HTTP delivery.
Only a successful JSON `{ "recorded": true }` acknowledgement clears pending,
with another durable write. An ambiguous acknowledgement replays the same fact.
A stop can supersede an unacknowledged open fact; the server accepts the complete
interval as its first report. Facts remain after deletion. Inventory maintenance
replays pending facts even if Boat itself is unavailable, then records newly
discovered lifecycle facts. Report HTTP failures leave pending facts and do not
prevent a preserving stop or destruction. Durable-write failure retains the
existing poisoned-journal fence. Docker-local keeps its existing lifecycle and
makes no VM-time requests.

## Historical backfill

Old v1 journals remain readable. A VM still in inventory can be backfilled from
its immutable creation request (selected size) and provider `createdAt`. A lost
create reply is recovered this way without repeating create. Unknown orphans
without size provenance, deleted old VMs, and VMs without `createdAt` require an
evidence-backed operator import. They remain unaccounted until supplied; no
current timestamp or zero is substituted for missing historical usage.

Stop the controller first so the import can acquire its exclusive Boat journal
lock. Gather historical size/start/end facts from retained provider/controller
evidence. Put a JSON array of bodies matching the example above in a local file,
using null for every unconfirmed end, then run:

```sh
aq-worker --config /private/worker.toml boat-vm-time-backfill \
  --controller-config /private/node.json --facts /private/vm-time-facts.json
```

The command uses the same enrolled node and Boat journal, validates local identity
and immutable-fact conflicts, journals imports before sending, and exits with an
error if delivery remains pending. Rerun the command to retry; already recorded
facts are unchanged. The server verifies attempt ownership and registered Boat
node scope. This command does not create, stop, or delete VMs. Imported history
is trusted operator evidence, not inferred lifecycle authority.

For attempts abandoned before any create intent, controller recovery proves
absence with a complete owned inventory and its durable pre-create phase. It
journals a stable `not-created/<attempt-id>` zero interval. Recovery retries
across a crash between abandonment and the accounting write. A later create
for that attempt is refused. An uncertain create intent or an empty inventory
alone never qualifies; imports of synthetic no-create facts are refused. Missing
historical evidence therefore continues to fence server placement as required.

## Focused verification

Run Cargo through the existing `aq test` Python wrapper with
`CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`:

```sh
cargo test --locked -p aq-worker --test boat_backend --test controller_cli
cargo test --locked -p aq-worker --lib controller::
cargo fmt -p aq-worker -- --check
```

`tests/support/boat_vm_time.rs` uses a loopback fake server to verify node bearer
and Access headers, create and stop reports, immutable duplicate replay, lost
create response, lost acknowledgements, restart after VM deletion, pending
acknowledgement state, legacy journals, historical import/CLI retry, and
controller-proven no-create facts. Existing Boat and controller cases cover the
unchanged lifecycle policy. The paid live test remains opt-in and is not part of
this task's validation.

Validation on 2026-11-03: 19 Boat tests, one controller CLI test, and 66 controller
unit tests passed; the paid Boat test was ignored. The seven VM-time cases are
included in those 19 Boat tests. Formatting and diff whitespace checks passed.
