# Terminal reporting after native stop: aq-admission 0.3.1

Task: `clear-grove-63`, cross-repository follow-up for aq-server B2
(`noble-summit-37.2`).

`DelegatedPermit::validate_for` now permits execution-scoped `ReportResult`
when trusted state is `GrantPhase::Ended`. The existing checks still require
`issued_at_ms <= now_ms < expires_at_ms <= grant_expires_at_ms <= hard_deadline_ms`.
No deadline is extended. Every other operation remains refused in Ended,
including renewal, material access, Started, Terminal, Prepared and mailbox
poll/ack. Preparing and Granted behavior is unchanged.

For an exact retry of the native stopped report, the server calls
`DelegatedPermit::validate_stopped_replay` with the retained original report
and the incoming report. This checks the permit's current reporting authority,
validated canonical report digest equality and attempt/generation/node/boot
correlation. Changing evidence or operation ID returns `replay_conflict`; an
expired exact retry returns `expired`. The retained original must come from
server state after authentication, validation against Started, and atomic native
stop persistence. Caller-supplied history is not authority. The server retains
and returns the original receipt; this helper does not persist it. Result
recording continues to require independently verified stopped proof.

## Source publication and B2 consumption

Pin `aq-admission = "=0.3.1"` using
`protocol/releases/aq-admission-0.3.1.tar.gz` and its adjacent JSON manifest.
The archive SHA-256 is
`ef9ae73d57b2faa6ededff7240095f69d6ef4c043312031c02720dea755814a8`.
The manifest records all 120 source member hashes. The task close summary records
the published git revision. The 0.2.0 and 0.3.0 archives remain unchanged.

The package has no wire schema changes. Its shared sandbox fixture set now has
125 vectors, including 24 terminal reporting vectors: allowed Ended result,
expiry and hard deadline boundaries, refusal of all twelve other operations,
preparation-scope refusal, exact stopped replay, changed evidence/operation,
expired replay and foreign binding. B2 should run the published vectors through
`sandbox::validate_fixture`, update its pin, and use the stopped replay validator
on the retained-stop retry path. aq-worker's `permit_fixture_conformance` pins
0.3.1 and requires these vector names.

## Verification

Tests run through the existing `docs/implementation/B1-evidence/test_cargo.py`
bridge under `aq test` resource controls (`CARGO_BUILD_JOBS=2`).

```sh
B1_CARGO_COMMAND='cargo +1.96.1 test --locked -p aq-admission@0.3.1 --test sandbox --test stopped --test result_record' B1_CARGO_LOG=docs/implementation/terminal-report-evidence/admission-tests.log aq test docs/implementation/B1-evidence/test_cargo.py -q
B1_CARGO_COMMAND='cargo +1.96.1 test --locked -p aq-worker --test permit_fixture_conformance' B1_CARGO_LOG=docs/implementation/terminal-report-evidence/worker-conformance.log aq test docs/implementation/B1-evidence/test_cargo.py -q
python3 scripts/package-admission.py --check
```

The admission checks pass 18 Rust tests (11 sandbox, 4 stopped, 3 result record),
including all 125 published vectors. The worker conformance test also passes
(1 Rust test). Evidence:
[admission tests](terminal-report-evidence/admission-tests.log),
[worker conformance](terminal-report-evidence/worker-conformance.log).
The archive reproducibility check passes; independent archive and member SHA-256
checks match the manifest. `git diff --check` and focused Rust formatting checks
pass.
