# Original-boot stopped replay: aq-admission 0.3.2

Task: `fleet-flare-68`, following the aq-platform B2 report from session
`ad314c0d`. This is a source-contract release for the aq-server B2 pin.

`DelegatedPermit::validate_stopped_replay` accepts the exact retained native
stopped report after the delegated permit has been re-adopted onto a new boot.
The historical report keeps its original boot. The current permit must still
match the trusted current `PermitContext` in full, including boot, sandbox and
channel, and remain within permit, grant and hard deadlines.

The server must authenticate the request's current credential/incarnation
independently, load the original report from retained server state, and return
its retained receipt. That original must have been authenticated, validated
against Started and atomically recorded with the native stop. Neither a
caller-supplied original nor a caller-supplied context establishes authority.

Canonical report digest equality protects the historical boot, operation ID and
evidence. Rewriting the report to the rebound boot or an arbitrary boot returns
`replay_conflict`; a stale or forged permit boot returns `binding_mismatch`.
Attempt, generation and node must still match the current permit. Exact retries
at expiry return `expired`. `validate_for` is unchanged: only execution-scoped
`ReportResult` remains permitted in Ended, and every other operation is refused.

## Source publication and consumer pin

Pin `aq-admission = "=0.3.2"` from
`protocol/releases/aq-admission-0.3.2.tar.gz`, using its adjacent JSON manifest.
Archive SHA-256:
`89cc29b239d849ac968d3a57d86420961062009dc966928d47fd91411132e1d8`.
The manifest records all 120 source member hashes. The 0.2.0, 0.3.0 and 0.3.1
archives remain unchanged. No API signature or wire schema changes are required.

B2 should update its source pin and run all 132 shared sandbox vectors through
`sandbox::validate_fixture`. Seven new vectors cover original-boot replay after
re-adoption, stale permit/context boots, forged permit boots, rewritten report
boots (arbitrary and rebound), and expired replay. The aq-worker consumer pins
0.3.2 and requires all seven names in `permit_fixture_conformance`.

The referenced consumer document `aq-server/docs/implementation/B2.md` was not
available in the local aq-server checkout or its `origin/main` at implementation
time. This note supplies the pin and caller requirements for that owner.

## Verification

The new regression failed on 0.3.1 with `BindingMismatch` for exact original-boot
replay; same-boot replay and negative cases passed ([red log](stopped-replay-readoption-evidence/red.log)).
The focused 0.3.2 admission checks pass 22 Rust tests: 15 sandbox, four stopped,
and three result-record tests, including all 132 vectors. The new tests exercise
a validated re-adoption with credential rotation, exact historical replay,
stale/forged boots, expiry, all twelve forbidden Ended operations and
preparation-scope refusal.

Commands run through the existing pytest bridge under `aq test` resource
controls (`CARGO_BUILD_JOBS=2`):

```sh
B1_CARGO_COMMAND='cargo +1.96.1 test --locked -p aq-admission@0.3.2 --test sandbox --test stopped --test result_record' B1_CARGO_LOG=docs/implementation/stopped-replay-readoption-evidence/admission-tests.log aq test docs/implementation/B1-evidence/test_cargo.py -q
B1_CARGO_COMMAND='cargo +1.96.1 test --locked -p aq-worker --test permit_fixture_conformance' B1_CARGO_LOG=docs/implementation/stopped-replay-readoption-evidence/worker-conformance.log aq test docs/implementation/B1-evidence/test_cargo.py -q
python3 scripts/package-admission.py --check
```

Evidence: [admission checks](stopped-replay-readoption-evidence/admission-tests.log),
[worker conformance](stopped-replay-readoption-evidence/worker-conformance.log).
Archive/member SHA-256 verification, preservation of all 125 prior fixtures,
focused Rust formatting and `git diff --check` also pass.
