# F1a host: taskless central session contract

`aq-session-host` 0.1.6 introduces `HostPlan` version 1, `HostSubject`,
`HostStartup`, `HostAdapter`, and `SessionHost::launch_plan`. Contract ownership
is aq-worker, as confirmed in `crisp-nexus-12` reopen feedback. The normative
consumer requirement is aq-server's
`docs/implementation/AQ-17-central-authority-core.md`, especially its neutral
adapter seam. This replaces the earlier blocked checkpoint at `658b233`.

## Consumer pin (0.1.7 race fix)

The reproducible publication is
`protocol/releases/aq-session-host-0.1.7-source.tar.gz`:

- SHA-256: `ab37cb192870f07290b4aff887b57b73783931ba5dfc248184da483c99036bdb`
- Size: 2,638,863 bytes.
- Receipt: `protocol/releases/aq-session-host-0.1.7-source.json`, including a
  SHA-256 and byte length for every source file.
- Package: `aq-session-host = 0.1.7`; HostPlan version: `1`.
- Unchanged wire dependency: `aq-session-protocol = 0.1.0`.

Extract the archive as a unit. It includes the locked workspace, all local
path dependencies, session-host's helper source, and `aq-codex-mapper`, avoiding
the missing packages in the older worker archive. Its source root is
`aq-session-host-0.1.7-source/`. Build the helper from that root with
`cargo build --locked -p aq-session-host --bin aq-session-host --jobs 2`.
The library is at `crates/aq-session-host`; a consumer vendoring this closure
must retain relative path dependencies and patch `aq-session-protocol` to the
included `crates/aq-session-protocol` for the terminal-relay dependency.
Do not substitute another helper version for the library: both read the same
private sealed configuration. The archive is source publication, not a
qualified native runtime image or a provider binary release.

Reproduce/verify with `python3 scripts/package-session-host.py --check`.
The script excludes logs, credentials, build output and sibling checkouts;
it includes tracked and new source files under the declared workspace roots.
The receipt pins the actual source bytes rather than a mutable branch.

## Contract and caller responsibilities

`HostPlan::decode` accepts at most 65,536 bytes, rejects unknown fields and
versions, and validates cross-field constraints. `HostPlan::digest` uses the
existing compact UTF-8, recursively sorted-key SHA-256 convention. The closed
subject variant is `central_conversation { actor_id, conversation_id, conversation_revision }`;
the physical `SessionIdentity::Central` carries authority, session and generation.
The logical subject has no project, graph, task or attempt identity.

The plan binds immutable `policy`, `execution`, mailbox batch,
adapter/environment and startup descriptor/payload pins, exact dispatch and
host-input operation UUIDs, plus resource and terminal limits. It deliberately
contains no digest of CentralPlan or DispatchAssociation: CentralPlan already
pins the HostPlan, so a back-reference would create a digest cycle. Construct
HostPlan first from the immutable batch, operations, policy, session and admitted
clock/deadline, then pin it in CentralPlan. F1a joins the resulting CentralBinding
and DispatchAssociation to these exact fields and translates its authenticated
PermitData into ExecutionPermit with the HostPlan digest. A renewal retains the
original HostPlan and deadline; it changes only the host permit. F1a resolves/authenticates those references and fences policy,
actor/session generation and dispatch before calling the host. The execution
artifact is the exact resolved composition/model/scopes/budget configuration;
the trusted pinned `HostAdapter` must implement it or refuse. This layer does
not copy the profile compiler or model selector. The existing subject-neutral
`aq-codex-mapper::Settings` recipe may be used by a qualified caller adapter;
recipe metadata itself is not execution or credential authority.

Call `launch_plan` with the validated plan, literal startup bytes, an
`ExecutionPermit`, canonical caller-owned scratch directory and local helper /
tmux installation. The adapter returns `AdapterLaunch` with exact plan digest
(in the retained `manifest_digest` field), adapter, CLI, environment and
capabilities. A mismatch refuses before durable intent or guardian creation.
`ResourcePolicy` retains its existing distinction between caller admission and
an exact installed resource-enforcement qualification.

Startup is one immutable UTF-8 payload, at most 1 MiB. The host checks its exact
size/digest, durably stages `context-0.bin`, and verifies it again in the pane
helper immediately before native exec. The adapter consumes that file through
`AQ_SESSION_DIRECTORY`; the host does not interpret its prompt. Additional
constituent context is refused. Live terminal input is disabled in this version.
Running proves physical host startup, not native readiness, stdin handoff,
mailbox dispatch, reply authority or native resume. An adapter's readiness is
an independent observation. Central credentials/private material and native
provider qualification remain caller-owned; the task-bound private-input and
observed factories refuse this new document.

## Authority, durability and restart

The plan records the original Linux machine boot UUID, CLOCK_BOOTTIME admission
and fixed deadline. Its wall limit must equal deadline minus admission. Every
permit binds the complete canonical plan and exact physical session, lasts at
most 120 seconds, and cannot start before admission or expire after the original
deadline. The host and guardian check the current boot; clock regression stops
the guardian. Renewal requires a live prior permit, the next sequence, a later
expiry and non-regressing issuance. Identical live renewal retries do not extend
authority. Cancellation/expiry is evaluated before renewed authority can apply.

The existing durable `LaunchAuthority` callbacks are shared, in order:

1. `record_launch_intent`, after sealed configuration and startup files are durable.
2. `record_guardian`, with exact process incarnation, nonce, boot and namespace.
3. `record_start_intent`, before publishing the physical start request.
4. `record_started`, after the host reports Running.

Callbacks must return only after caller-owned state is durable. Their errors
propagate. An error after guardian creation can leave physical ownership alive
until explicit recovery or permit expiry; it never authorizes replay. Retain the
launch/guardian records even when `launch_plan` returns an error. Launch directory
creation is exclusive and launch IDs are never reused.

`SessionHost::observe_existing` requires the exact recorded guardian and an
unexpired Running host. It neither starts processes, emits launch callbacks nor
renews authority. `recover` remains stop-and-preserve. `stopped_proof` returns the
same opaque `StoppedProof` only after the exact guardian's durable kernel ECHILD
receipt. Guardian loss is StopUncertain even when known processes disappear;
it cannot authorize a successor. F1a owns durable retention, successor admission
and dispatch uncertainty reconciliation using those receipts.

## Compatibility and 0.1.6 verification

The v1 LaunchManifest validator and protocol source are unchanged. Existing
`launch`, `launch_rendered`, observed/private factories and their public adapter
interfaces remain available. The private sealed document union serializes old
manifests identically, preserving old configuration and manifest digests. There
are no placeholder task IDs or weakened nil-ID checks.

Cargo test/check execution uses `aq test` with the existing
`docs/implementation/B5a-evidence/test_cargo.py` wrapper and
`CARGO_BUILD_JOBS=2`. Exact command/result receipts and logs are retained under
`docs/implementation/F1a-host-evidence/`. Final qualification results are recorded
below. Initial development failures (consumer version pin, mechanical tuple
conversion, test-root permissions, a too-short expiry-test permit, and Clippy's
argument-count lint) remain in the evidence logs; none are claimed as passes.
The launch API retains the existing explicit launch boundary plus literal startup
bytes; a documented Clippy expectation covers only that argument-count lint.

| Check | Result / evidence |
| --- | --- |
| `cargo test --locked --workspace supervisor_session` | Passed, 7 taskless tests; `t1.json` / `t1.log`. The only subsequent Rust change is the argument-count lint annotation. |
| Extracted release: `cargo test --locked -p aq-session-host --all-features` | Passed, 68 tests (59 existing opt-in tests ignored); `artifact-host-final.json`. Includes unit, synthetic host, taskless physical and doc tests. |
| Extracted release: `cargo test --locked -p aq-session-host --all-features --test local_tmux central_and_remote_interaction_stop_and_no_relaunch -- --ignored` | Passed, both legacy session identities exercised in one test; `legacy-local.json` / `legacy-local.log`. |
| Final extracted release: `cargo clippy --locked -p aq-session-host --all-targets --all-features -- -D warnings` | Passed; `clippy.json` / `clippy.log`. |
| `cargo fmt -- --check`, `git diff --check` | Passed. |
| `python3 scripts/package-session-host.py --check` | Exact archive and complete per-file receipt reproduced. |

The extracted-release receipts include their exact `--manifest-path`, resolving
only inside the unpacked archive. New tests have no ignores or provider access.
CI explicitly installs tmux for the local physical qualification. This evidence
does not qualify the aq-server host container, conversation dispatch, central
credentials or a native Codex image; those remain F1a/F1b integration work.


## 0.1.7 adoption for aq-platform F1a

For `steady-harbor-87.1`, replace the 0.1.6 source closure with the exact 0.1.7
archive above, verify its receipt/hash, and pin `aq-session-host = "=0.1.7"`.
Rebuild and install the helper from that same closure before admitting new
central sessions. Preserve the existing session-protocol 0.1.0 dependency and
HostPlan version 1. Do not mix library/helper source pins or infer that an old
already-running helper was upgraded by changing a dependency.

0.1.7 fixes a publication race: the guardian previously observed a final
`start.request` path before `write_all` completed, then correctly stopped on
partial JSON. `durable_new` now writes a unique mode-0600 file in the same
directory, fsyncs its complete contents, publishes with Linux
`renameat2(RENAME_NOREPLACE)`, then fsyncs the directory. An existing file or
symlink wins without replacement. Failed writes/publications remove their owned
temporary file; a crash can leave an unpublished temporary file, which the
guardian never consumes. Unsupported kernel/filesystem rename semantics refuse;
there is no non-atomic fallback. Published malformed JSON remains a fatal startup
error and cannot launch tmux.

Admission remains caller-owned: authenticate/fence the dispatch, retain durable
launch/guardian/start callbacks, bind the exact plan/session and capacity
reservation, and issue the initial permit within the original boot/admission/
deadline bounds. A failed/ambiguous launch still requires recovery and exact
stopped-writer proof before successor admission; atomic publication adds no
replay or restart authority. Qualify the upgraded aq-platform composition in its
own physical tests; host component checks do not establish native provider or
central-container qualification.

0.1.7 verification and exact command receipts are retained in
`docs/implementation/start-publication-0.1.7-evidence/`. The 0.1.6 evidence and
publication remain historical artifacts, not evidence for the new bytes.


| 0.1.7 check | Result / evidence |
| --- | --- |
| Atomic slow writer, no clobber/cleanup, competing publishers | 3 passed; `atomic.json`. |
| Legacy publication injected in disposable extraction | Expected regression failure detected; exact source restored; `legacy-race.json` / `legacy-race.log`. |
| `cargo test --locked -p aq-session-host --all-features` | 73 passed, 59 existing opt-in/fixture ignores; `host.json`. Includes 50 fresh central starts. |
| Same suite from final extracted release | 73 passed, 59 existing ignores; `artifact-host.json`. Includes another 50 starts and malformed-start refusal. Initial existing credential-lock test failure is retained in `artifact-host-initial-lock-failure.*`; exact focused rerun and unchanged area retry passed. |
| Host all-target/all-feature Clippy with `-D warnings` | Passed; `clippy.json`. |
| Extracted file/archive integrity | 1,201 file hashes/lengths plus archive hash/size verified; `artifact-integrity.json`. |
| Workspace formatting, whitespace, publication reproduction | `cargo fmt -- --check`, `git diff --check`, `python3 scripts/package-session-host.py --check` passed. |

These receipts are relative to `start-publication-0.1.7-evidence/`. Its README
records exact reproduction details and limitations. The original aq-platform
physical test is not available in this checkout; the host's equivalent central
launch loop is qualified here, while F1a consumer qualification remains with
aq-platform.
