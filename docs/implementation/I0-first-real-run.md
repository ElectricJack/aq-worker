# I0: first real ChatGPT native run qualification

Task `azure-horizon-22.1`, observed 2026-11-02. **Acceptance is unmet: 0 of 3 real runs.**
No provider request, frozen attempt, execution grant, repository edit, result retention,
human review decision or completion reconcile is claimed. The owner's login has not
been copied, delivered or changed. No refresh occurred because no launch occurred.

## Prepared and verified

The source baseline and image worker source are current-main revision
`dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5`. The plan was committed first as
`13fadfe`; initial helper/tests were committed as `3ba25b4`, followed by the
hardening recorded below. Final publication identifies the evidence branch revision.
Only aq-worker files were edited; aq-server's worker candidate was not repinned.

The isolated build copied `Dockerfile.worker` and its pinned Git inputs from E0's
archived aq-server source `8f6c49fd91850b924524494e26c5c17d14072796`, replaced only
the isolated worker source archive with `git archive origin/main`, and added the
native helper/result client, installed Codex ELF, ripgrep, tmux 3.4 and runtime
libraries. No operator home or credential entered the build context.

Final local image:
`sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d`.
It reports worker 0.5.1, Codex CLI 0.166.0, tmux 3.4 and Git 2.56.0.
[Build inputs](evidence/i0-first-real-run/build-inputs.json),
[Dockerfile](evidence/i0-first-real-run/Dockerfile.worker),
[image identity](evidence/i0-first-real-run/image.json) and
[installed file pins](evidence/i0-first-real-run/installation.json) are retained.
The executed final build command, using the retained credential-free context, was:

```sh
docker build --progress=plain --target i0 \
  --build-arg AQ_SOURCE_REVISION=dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5 \
  --build-arg AQ_WORKER_ARCHIVE_SHA256=38dfa8fed216d7bc2621d68d58550849714fdd009db4006fadfffb743356ef01 \
  -t aq-i0-worker:dc62cfc /home/jkern/.local/state/aq-platform-i0/build
```

The image is local and was not published to a registry. Debian package repositories
and the original unqualified Rust build-stage tag prevent a bit-for-bit rebuild
claim; the observed final image ID is immutable.

Environment: WSL2 Linux `6.18.33.2-microsoft-standard-WSL2`, x86_64;
Docker client/server 29.3.1, Compose v5.1.1; Rust 1.96.1; Python 3.12.3.
Build jobs were capped at two for the worker and tmux extension. No test-support
feature, fake provider or provider override was built into the worker image.

The existing bootstrap's `inspect` mode ran in the final image with host networking,
UID/GID 10001, read-only root, all capability sets empty and no-new-privileges.
It verified single-link program files and a separate persistent state filesystem.
[Physical observation](evidence/i0-first-real-run/physical-preflight.json) is metadata,
not an independent runtime/model qualification or a `check-native` success.
The stopped credential-free inspector containers and their empty state are retained.

A credential-free `codex sandbox -- /bin/true` probe with private writable
`CODEX_HOME` under `/state`, a writable `/tmp`, read-only root, dropped capabilities
and no-new-privileges failed under Docker's default seccomp policy. Adding
`--security-opt seccomp=unconfined` returned exit 0 with the other constraints intact.
[Exact corrected requests/results](evidence/i0-first-real-run/sandbox-probes-corrected.json)
are retained. An earlier probe used obsolete `sandbox linux` syntax; the installed
CLI's help corrected it, and that invocation is not counted as a valid sandbox test.
Probes with a read-only default home or a home under `/tmp` also exposed alias setup
requirements; the successful probe uses a private home outside `/tmp`.
The mapper's interactive argument prefix parses successfully with `--help`
([result](evidence/i0-first-real-run/interactive-argv-help.json)); this is parsing,
not a model call. These probes make no hostile-code isolation claim.

## Blockers and owner decision

E0's final stack is retained at `/home/jkern/.local/state/aq-platform-e0-final`;
all four services were healthy. Admin, worker and Graph URLs are respectively
`http://127.0.0.1:32838`, `http://127.0.0.1:32839` and
`http://127.0.0.1:32837`. The designated scratch repository is
`https://github.com/ElectricJack/aq-scratch.git`, public branch `main`, credential
reference `none`. E0 retained base `2e32645bab743729bfaeb0f2eec417c2ace7794b`
under handle `e0000000-0000-4000-8000-000000000011`. That unchanged E0 base is
not I0 output.

The active profile catalog is revision 1, digest
`0c8a041de2e1af9e6937cd0c87cefed8a0c82d6e592d986c7d0ab3dfc817d6f2`.
Its provider/model are `configuration-required` and credential references are empty.
E0 vault configuration has no intelligence, profiled_execution, playbooks or
terminals owner configuration. Current native admission requires exact qualified
runtime/harness/environment/scoped-presence inputs and matching model/credential
context counting and capacity evidence. The checked-in physical fixture evidence
explicitly excludes production model/environment qualification. Arbitrary fixture
hashes were not installed as real qualification. Production `check-native` and
`run-native` were therefore not attempted with invented configuration.

The completion reconcile endpoint returns HTTP 404 `no such route` for a nonnil
operation ID and `{}`. Source inspection confirms the routes are installed with an
enabled human-review playbook runtime; E0 omits it. That owner also requires a
protected engine keyring/connection and trusted Unix PostgreSQL socket, absent
from this handoff. [Owner route observations](evidence/i0-first-real-run/owner-route-preflight.json)
include a `/dependency-readyz` request sent to the admin listener; its 404 is not
counted as a readiness defect because that probe used the wrong listener.

Supervisor requests `msg-a3c7cdd4da6d48c798c4dd8556d42d4b` and
`msg-0780e4861b3d42b996ce272471e25784` request upstream merge substitution and
qualified setup location/guidance. Owner dashboard message
`msg-ab792460ea124c03b8edf522083d613f` requests the missing qualified inputs or
an explicit bounded qualification decision, plus settlement runtime ownership.
No reply or waiver has been recorded at this report revision. The imported
`aq-platform` message direction was refused by the live `aq-worker` project scope;
all subsequent requests used the authorized project. Wait registration is disabled
for this project; no unsupported durable wait was registered.

## Login reconciliation helper and verification

`scripts/i0-auth-writeback.py` is manual qualification glue. It validates bounded
private regular login files, refuses symlinks and duplicate JSON fields, compares
an initial snapshot with the current owner file, writes exact refreshed bytes to a
0600 sibling, fsyncs, atomically renames and fsyncs the directory. It prints status
only. Its schema matches native login validation, including required bounded account ID,
ASCII token bounds and rejection of unknown fields. Changed owner bytes cause refusal with both inputs preserved. Missing launch
output and unchanged login preserve the owner file. The helper has only been used
with synthetic credentials. It does not coordinate arbitrary concurrent writers:
there is a compare/rename race, so owner writers must be quiescent during manual
reconciliation. It does not implement a vault or update the server material registry.
No container with real credentials may be removed until source and login state are
reconciled; retain it and its private state on any conflict or uncertain failure.

An actual final-image setup failure and helper owner-conflict refusal used synthetic
login files only. Both returned exit 2, all private files remained byte-identical
at mode 0600, and Docker inspection confirmed the exited container was retained
with auto-remove disabled. [Request and observations](evidence/i0-first-real-run/failure-preservation.json)
are retained. This verifies setup/refusal preservation before admission; it does
not verify post-launch real credential or source reconciliation.

Commands and observed results:

| Verification | Result |
| --- | --- |
| `aq test tools/qualification/tests/test_i0_auth_writeback.py -q` before helper | RED: 6 failed, helper absent |
| Same command after initial helper | GREEN: 6 passed |
| Added aged-atime and duplicate-field cases | RED: 2 failed, 6 passed |
| Same command after hardening | GREEN: 8 passed |
| Added native-invalid login cases after fresh review | RED: 10 failed, 8 passed |
| Same command after native-schema validation fix | GREEN: 18 passed |
| Actual synthetic setup/conflict failure in final image | PASS: exit 2, private files unchanged, container retained |
| `python3 -m py_compile scripts/i0-auth-writeback.py tools/qualification/tests/test_i0_auth_writeback.py` | PASS |
| `git diff --check` | PASS |
| Docker build, `CARGO_BUILD_JOBS=2 cargo build --locked --release --bins` | PASS, final image above |
| Final-image worker/Codex/tmux/Git version checks | PASS |
| Existing bootstrap `inspect` on final image | PASS, metadata only |
| Correct sandbox probe, default seccomp | FAIL, bubblewrap namespace refusal |
| Correct sandbox probe, seccomp unconfined | PASS, no provider call |
| Mapper argument prefix with `--help` | PASS, no provider call |
| `cargo fmt --all -- --check` in nested worktree | FAIL, vendored crate resolves outer workspace |
| Same fmt command from standalone current-main archive | FAIL, existing import order in `native/result/source.rs` |

AQ's Python wrapper selected its configured fallback interpreter and used its normal
six-worker/default marker configuration. Test logs are `/tmp/i0-auth-red.log`,
`/tmp/i0-auth-green.log`, `/tmp/i0-auth-hardening-red.log` and
`/tmp/i0-auth-hardening-green.log`, `/tmp/i0-auth-review-red.log` and
`/tmp/i0-auth-review-green.log`. Build logs are `/tmp/i0-image-build.log`,
`/tmp/i0-image-tmux-build.log`, `/tmp/i0-image-final-build.log`; fmt logs are
`/tmp/i0-fmt.log` and `/tmp/i0-fmt-standalone.log`. Persistent build inputs and
observations are under `/home/jkern/.local/state/aq-platform-i0`.
No Rust source changed; additional full workspace Clippy/test suites were not run.
The required real-provider acceptance remains unverified.

## Filed findings and remaining acceptance

| v1 task | Confirmed finding |
| --- | --- |
| `sharp-harbor-18` | Missing real native qualification/configuration inputs |
| `quick-glacier-78` | Human settlement runtime/routes absent from local stack |
| `fresh-delta-13` | Changed ChatGPT login is preserved by cleanup refusal; durable owner/server reconciliation missing |
| `vivid-bridge-32` | Imported project/supervisor directions exceed live project scope |
| `amber-horizon-91` | Native image needs current worker, helper/client, pinned tmux/Codex and full shared-library dependencies |
| `stark-grove-17` | Baseline formatting and nested-worktree metadata failures |
| `sharp-dune-88` | Default Docker seccomp blocks the actual credential-free Codex sandbox probe |

The helper defects found during TDD and fresh review were fixed and are covered
by the final 18-case suite; durable reconciliation remains tracked separately.

A fresh whole-branch reviewer found two important gaps: native-schema parity and
missing actual early-failure preservation evidence. The single fix pass reproduced
ten invalid-login acceptances before tightening validation, then passed all 18
tests. The synthetic Docker failure probe above addresses the bounded setup case.
There were no critical or minor findings. The review supports publishing partial
preparation only; all real-run acceptance remains unmet.

Decisions retained from the execution ledger, in order:

1. Do not promote fixture hashes to genuine qualification; owner inputs or an
   explicit bounded qualification method are needed. Cost if wrong: delayed runs.
2. Build pinned tmux 3.4 because the native host requires it. Cost if wrong:
   unnecessary image-build work.
3. Report unmet acceptance without inferring end-to-end success from unit tests.
   Cost if wrong: an overly conservative task result.
4. Close fail/blocked when owner inputs remain unavailable, as the task requires.
   Cost if wrong: the owner must resume the task.
5. The reviewer's real provider/source/settlement/rotation exclusions remain unmet
   acceptance, so this evidence cannot pass I0. Cost if wrong: delayed qualification.
6. Manual writeback requires quiescent owner writers; durable multiwriter support
   is follow-up `fresh-delta-13`. Cost if that precondition is violated: lost rotation.
7. Exact observed image identity is sufficient for these probes; floating build
   dependencies prevent a reproducibility claim. Cost if wrong: rebuild divergence.
8. Root inspection and AQ receipts support external E0/configuration/task claims
   the reviewer did not re-query. Cost if wrong: stale external evidence.
9. Plain Docker probes make no hostile-code isolation claim. Cost if treated as
   such a boundary: insufficient isolation.
10. The assigned publication/close instructions govern handoff; no whole CI run,
    shared-branch merge or extra approval menu is needed. The absent `origin/dev`
    and unanswered substitution request leave the upstream merge unmet. Cost if
    wrong: delayed integration or broader checks needed later.

| Run | Attempt/grant | Codex/model | Wall time | Retained source | Settlement | Refresh |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |
| 2 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |
| 3 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |

To resume, supply the missing qualified configuration and enabled settlement owner,
then run production `check-native` against actual installed pins before delivering
login through the existing material lease. Each real invocation needs its own fresh
empty single-slot state root. Preserve original and refreshed private login files,
reconcile owner state before disposal, retain edits centrally, obtain actual human
review input, and reconcile each completion through the specified public routes.
The task must close fail unless all three consecutive runs are evidenced.
