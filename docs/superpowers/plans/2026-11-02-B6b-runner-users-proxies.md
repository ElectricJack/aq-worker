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
- [x] Preserve already-reviewed/published feature history and integration ancestry; merge the configured integration source and prepare guarded publication and explicit close with evidence. `origin/dev` is absent; the updated project `.aq/PRIME.md` explicitly requires `origin/main`.

**Historical claim-epoch-1 qualification outcome:** Passing publication was blocked by the unmet supervisor conditions (empty agent bounding set and explicit outside-pane signal-denial proof), as well as complete-suite failures. Three complete-suite attempts failed in unmodified driver/journal fixtures, including a serial attempt ending with 239 passed, 2 failed and 18 ignored in worker unit tests. Both exact security acceptance filters pass, and the sandbox-runner compatibility suite passes 12/12 after updating its explicit prerequisite-refusal assertion. Preserve the implementation/review-fix history and synced integration ancestry, push the task branch for recovery, and close fail with the complete evidence rather than marking the passing publication step complete. Independent deterministic-fixture follow-up: `noble-impact-55`.

### Recovery Task 4: Supervisor security conditions (claim epoch 2)

**Files:** `crates/aq-worker/tests/support/b6b_user_probe.rs`, `crates/aq-session-host/src/users.rs`, `scripts/test-b6b-root.sh`, and this evidence/plan.

**Interfaces:** carry the live numeric runner PID as a nonsecret fixture argument to both `agent-probe` and the tmux harness. `agent_probe(root: &Path, runner_pid: libc::pid_t)` verifies the target's runner UID, then requires `kill(runner_pid, 0)` and `kill(runner_pid, SIGCONT)` to return `EPERM`. The harness additionally verifies that its session differs from the runner's session. This checks an outside-pane target without destructive signals.

- [x] Add direct and real tmux signal-denial coverage. The standalone fixture initially exposed the same-session SIGCONT exception; it now uses setsid before the UID transition and both probes prove distinct sessions and EPERM, with no production code change.
- [x] Resolve the prerequisite with the supervisor: retain residual CapBnd=0xe0; no SETPCAP or bootstrap changes. Assert the exact residual in both fixture agents, and retain attributed root/agent EPERM evidence. The zero Inh/Prm/Eff/Amb and no_new_privs assertions remain enforced.
- [x] Preserve the approved privilege arrangement. Both exact named acceptance filters pass with one actual test each; security 8/8, runner compatibility 12/12, supervisor_session 7/7 and users 2/2 pass. RED outputs remain retained.
- [x] Record current recovery qualification separately from historical full-suite failures. Formatting and all-targets Clippy pass. Final sync with origin/main changed no files; source ebc4b27 includes main 5e05a02. Final documentation/log publication and the explicit close record preserve exact results; unrelated historical baselines are not rerun or changed.

**Pending owner decision:** `msg-d908840aa4d14e3e9ad1c4fd49bfdd79` asks whether an already-granted root bootstrap `CAP_SETPCAP` may clear the bounding set before retaining only SETUID/SETGID/KILL. Current minimal fixture lacks SETPCAP. Do not change that privilege prerequisite while approval is pending.

**Recovery integration RED:** the first exact required filter fails to compile on inherited HEAD `925eedc`: `launch_as_agent` still passes a bare `HarnessAdapter` after main introduced `host_plan::Adapter`, and `launch_plan` omits B6b's optional identity argument. Repair those two call sites with `Adapter::Task(adapter)` and `None`, respectively, before rerunning the acceptance filter. Files: `crates/aq-session-host/src/{lib.rs,host_plan.rs}`. This reconciles B6b with the already-merged API and adds no new launch behavior.

**Superseding supervisor ruling:** `msg-7ea45832655a419ea5723f75e6c1212f` drops CapBnd clearing from scope and explicitly forbids SETPCAP, bootstrap capability changes and backend/image policy changes. The existing `0xe0` bound remains, with `no_new_privs=1` and zero Inh/Prm/Eff/Amb. Preserve bright-nexus-60's actual root/agent `PR_CAPBSET_DROP(CAP_KILL) = -1, EPERM` probe as attributed prerequisite evidence. Replace the proposed CapBnd-clear RED/implementation steps above with an assertion that the minimal fixture retains exactly `0xe0`; no production privilege implementation change is authorized or needed.

**Signal test RED/GREEN:** real execution exposed Linux's same-session SIGCONT exception in the direct subprocess fixture. The standalone fixture now calls `setsid` before its agent transition. Both direct and real tmux probes require distinct runner/agent sessions, verify the target's live runner UID, and require both signal 0 and SIGCONT to fail with EPERM. The first exact workspace rerun passes one named user test; final qualification also covers the explicit residual CapBnd assertion and accurate capability wording.

**Recovery outcome:** both named acceptance tests and every scoped check pass under the superseding supervisor scope ruling. Final evidence: `docs/implementation/B6b.md` recovery section and `B6b-evidence/recovery-*.log`. No SETPCAP, bootstrap privilege, image, backend-policy, production transport or journal-fixture changes were made during recovery.

### Owner close feedback: one privileged-exec probe

Owner message `msg-9c640dff06c94d82b92702877b766033` replaces the earlier all-empty-bounding requirement with: zero Inh/Prm/Eff/Amb, no_new_privs set, CapBnd a subset of KILL/SETGID/SETUID, and one fast real exec probe showing those privileges cannot be regained. No SETPCAP or bootstrap changes; targeted named checks only.

- [x] Before bootstrap, copy the fixture executable into a root-owned directory, set the copy mode 04755 and the directory 0755, and verify root ownership/setuid bit. Pass its path only to the standalone agent probe.
- [x] After the existing agent probe, exec that root-owned setuid executable with a `suid-exec-probe` role. Require real/effective UID remain agent, NNP remain 1, and all four active capability sets remain empty. This is one privileged exec, with no production code or fixture capability changes.
- [x] Run the exact user acceptance filter: one actual test passed in 3.64s with the privileged-exec assertions. Formatting and incremental Clippy pass (2.26s). Final publication/close record carries this follow-up; no broad test suites were repeated.
