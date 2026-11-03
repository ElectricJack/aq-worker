# B6b Runner Users and Proxies Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep runner credentials private while an agent UID executes under the existing tmux guardian and reaches credential-bearing upstreams only through loopback.

**Architecture:** A runner-private layout owns bounded secret files and state. Reuse aq-responses-transport for provider forwarding and add a bounded AQ HTTP proxy with fixed upstream, stripped Access headers and sensitive-response refusal. Add an optional sealed agent identity to the session host; only the final pane exec changes identity. Runner bootstrap retains only the Linux privileges required to switch UID/GID and stop descendants; unsupported runtime capabilities refuse explicitly.

**Tech Stack:** Rust, Linux UID/GID and capabilities, Tokio, reqwest, existing session guardian and Responses transport.

**Spec:** AQ task noble-summit-37.8, B6b section of aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml; credential rules in architecture/0005 §7.

## Global Constraints

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Hosting:** every real agent CLI runs in an attempt-owned tmux host under the guardian (H1), inside its own sandbox (SBX1). Central supervisors run in the supervisor host (DEP5).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Isolation claims:** do not claim hostile-code isolation for shared-kernel containers (W6, SBX10).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- No seccomp, namespace or image-policy changes. C6a owns Codex configuration; C6b owns subscription material; B7 owns the agent credential. B6a2 owns production transport installation; preserve B6a1's prelaunch refusal until it exists.
- Verification uses Cargo jobs=2 and test threads=2 because aq test is a pytest wrapper without a Cargo backend. Run both exact workspace acceptance filters, affected worker/host suites, formatter and Clippy. Evidence: docs/implementation/B6b.md.
- No `review: adversarial` marker is present. One independent final review is required by executing-plans.

## Review Focus

- Symlinks, wrong ownership, broad modes and oversized secrets must refuse before forwarding.
- Spoofed Access headers and hop-by-hop headers cannot override fixed upstream authority or AQ bearer credentials.
- Upstream redirects, failures and echoed Access credentials must never disclose the runner token.
- Dropping UID must also clear supplementary groups and all capabilities; failed transitions cannot execute a harness.
- Cross-UID descendants must still stop under the existing guardian; inaccessible workspace ancestors must refuse rather than relaxing runner-private permissions.

---

### Task 1: Runner-only layout and loopback forwarding

**Files:** Create `crates/aq-worker/src/native/runner/sandbox/{private.rs,proxy.rs,security_tests.rs}`; modify `sandbox.rs`; create `crates/aq-worker/tests/sandbox_security.rs`.

**Interfaces:** `RunnerFiles::create(root: &Path) -> Result<Self>`, `write_secret(name, bytes)`, `read_secret(path) -> Result<Zeroizing<Vec<u8>>>`; fixed names permit.json, provider.key, access.json, state. `AqApiProxy::start(server: &str, access_file: &Path) -> Result<Self>`, `origin() -> &str`, `stop(self)`; production upstream HTTPS machine origin, test-only numeric loopback. `RunnerResponsesProxy::start(policy, provider_file)`, backed by the existing bounded scoped Responses transport.

- [x] Write `aq_proxy_injects_access_header_without_exposing_token`: real loopback upstream receives exact runner Access values and original agent Authorization despite spoofing; response never contains token, including echo/error/redirect cases.
- [x] Write private-file tests for exclusive 0700/0600 layout, symlink/wrong-mode/oversized rejection and secret-free Debug/errors. Run focused tests; expect missing behavior.
- [x] Implement bounded files and fixed-origin forwarding, no environment proxies or redirects, generic errors and response secret checks; reuse existing provider policy and retained credential validation.
- [x] Run tests including concurrent requests, cancellation, both loopback-only constructors and provider stream authorization. Commit.

### Task 2: Agent UID transition under tmux

**Files:** Create `crates/aq-session-host/src/users.rs`; modify `lib.rs`, `supervisor.rs`, `observed/mod.rs`; modify worker `sandbox.rs` and `launch/sandbox.rs`; test `crates/aq-worker/tests/sandbox_security.rs`.

**Interfaces:** `AgentIdentity { uid: u32, gid: u32 }`; `SessionHost::launch_as_agent(..., agent: AgentIdentity)` seals identity with command; pane clears groups, setgid, setuid and capabilities immediately before exec. `bootstrap_runner(runner, agent)` runs only in a fresh single-thread process before async runtime, retaining CAP_SETUID/CAP_SETGID/CAP_KILL for runner/guardian. `sandbox::run_as_agent` accepts an independently accessible workspace outside private state and uses launch_as_agent. Existing fixture runner retains its compatibility entrypoint.

- [x] Write `agent_user_cannot_read_permit_or_proxy_key`: real kernel UID transition in a child, independent numeric runner/agent identities, denied reads for permit/provider/Access/state/journal, empty supplementary groups/capabilities and no secret in environment/argv. Root-capable execution runs the same test binary, independent of aq-sandbox-os; unprivileged execution must invoke an explicit fixture wrapper rather than claim a skipped probe passed.
- [x] Add real guardian/tmux test exercising the separated pane and descendant stop; add invalid UID, failed drop and inaccessible workspace cases. Run RED.
- [x] Implement sealed identity and drop, bootstrap preconditions, private workspace separation and user lookup. Keep production CLI's unavailable-transport refusal; expose the bootstrap and separated library APIs for B6a2 without adding its server transport.
- [x] Run GREEN, existing sandbox runner tests and session-host tests. Commit.

### Task 3: Qualification and publication

**Files:** Create `docs/implementation/B6b.md`; update this plan.

**Interfaces:** consumes both implementations; produces named-test evidence and pushed branch.

- [x] Run exact workspace filters `agent_user_cannot_read_permit_or_proxy_key` and `aq_proxy_injects_access_header_without_exposing_token`, require nonzero passing tests. Run formatter, Clippy and related suites with bounded resources.
- [x] Record environment, precise commands/results, privilege prerequisites, production wiring limits, and commits in evidence.
- [x] Obtain fresh whole-branch review, resolve material findings with regression tests.
- [ ] Squash unpublished leaf implementation history, merge configured integration source, run targeted checks if changed, guarded push and close with evidence. `origin/dev` is absent; the updated project `.aq/PRIME.md` explicitly requires `origin/main`.

**Qualification outcome:** Passing publication is blocked by the unmet supervisor conditions (empty agent bounding set and explicit outside-pane signal-denial proof), as well as complete-suite failures. Three complete-suite attempts failed in unmodified driver/journal fixtures, including a serial attempt ending with 239 passed, 2 failed and 18 ignored in worker unit tests. Both exact security acceptance filters pass, and the sandbox-runner compatibility suite passes 12/12 after updating its explicit prerequisite-refusal assertion. Preserve the implementation/review-fix history and synced integration ancestry, push the task branch for recovery, and close fail with the complete evidence rather than marking the passing publication step complete. Independent deterministic-fixture follow-up: `noble-impact-55`.

### Recovery Task 4: Supervisor security conditions (claim epoch 2)

**Files:** `crates/aq-worker/tests/support/b6b_user_probe.rs`, `crates/aq-session-host/src/users.rs`, `scripts/test-b6b-root.sh`, and this evidence/plan.

**Interfaces:** carry the live numeric runner PID as a nonsecret fixture argument to both `agent-probe` and the tmux harness. `agent_probe(root: &Path, runner_pid: libc::pid_t)` verifies the target's runner UID, then requires `kill(runner_pid, 0)` and `kill(runner_pid, SIGCONT)` to return `EPERM`. The harness additionally verifies that its session differs from the runner's session. This checks an outside-pane target without destructive signals.

- [ ] Add and run the direct and tmux signal-denial assertions using the existing root fixture; expect PASS because cross-UID permissions already deny these signals. This is coverage for existing behavior, with no production code change.
- [ ] Once the owner resolves the bootstrap privilege prerequisite, add `CapBnd` to the existing zero-capability assertions. Run `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 TOKIO_WORKER_THREADS=2 cargo test --locked --workspace agent_user_cannot_read_permit_or_proxy_key`; expect RED on the nonempty bounding set before implementing the approved mechanism.
- [ ] Implement only the approved privilege arrangement, without editing image/backend policy. Re-run both exact named acceptance filters and the affected host/security suites; preserve all failed outputs.
- [ ] Update evidence to distinguish current recovery results from historical full-suite failures. Run formatting and Clippy for changed code, merge `origin/main` into the task branch, guarded push, and close with exact results. Historical unrelated baseline failures do not require another full-suite run under the current prime instructions.

**Pending owner decision:** `msg-d908840aa4d14e3e9ad1c4fd49bfdd79` asks whether an already-granted root bootstrap `CAP_SETPCAP` may clear the bounding set before retaining only SETUID/SETGID/KILL. Current minimal fixture lacks SETPCAP. Do not change that privilege prerequisite while approval is pending.

**Recovery integration RED:** the first exact required filter fails to compile on inherited HEAD `925eedc`: `launch_as_agent` still passes a bare `HarnessAdapter` after main introduced `host_plan::Adapter`, and `launch_plan` omits B6b's optional identity argument. Repair those two call sites with `Adapter::Task(adapter)` and `None`, respectively, before rerunning the acceptance filter. Files: `crates/aq-session-host/src/{lib.rs,host_plan.rs}`. This reconciles B6b with the already-merged API and adds no new launch behavior.
