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

- [ ] Write `aq_proxy_injects_access_header_without_exposing_token`: real loopback upstream receives exact runner Access values and original agent Authorization despite spoofing; response never contains token, including echo/error/redirect cases.
- [ ] Write private-file tests for exclusive 0700/0600 layout, symlink/wrong-mode/oversized rejection and secret-free Debug/errors. Run focused tests; expect missing behavior.
- [ ] Implement bounded files and fixed-origin forwarding, no environment proxies or redirects, generic errors and response secret checks; reuse existing provider policy and retained credential validation.
- [ ] Run tests including concurrent requests, cancellation, both loopback-only constructors and provider stream authorization. Commit.

### Task 2: Agent UID transition under tmux

**Files:** Create `crates/aq-session-host/src/users.rs`; modify `lib.rs`, `supervisor.rs`, `observed/mod.rs`; modify worker `sandbox.rs` and `launch/sandbox.rs`; test `crates/aq-worker/tests/sandbox_security.rs`.

**Interfaces:** `AgentIdentity { uid: u32, gid: u32 }`; `SessionHost::launch_as_agent(..., agent: AgentIdentity)` seals identity with command; pane clears groups, setgid, setuid and capabilities immediately before exec. `bootstrap_runner(runner, agent)` runs only in a fresh single-thread process before async runtime, retaining CAP_SETUID/CAP_SETGID/CAP_KILL for runner/guardian. `sandbox::run_as_agent` accepts an independently accessible workspace outside private state and uses launch_as_agent. Existing fixture runner retains its compatibility entrypoint.

- [ ] Write `agent_user_cannot_read_permit_or_proxy_key`: real kernel UID transition in a child, independent numeric runner/agent identities, denied reads for permit/provider/Access/state/journal, empty supplementary groups/capabilities and no secret in environment/argv. Root-capable execution runs the same test binary, independent of aq-sandbox-os; unprivileged execution must invoke an explicit fixture wrapper rather than claim a skipped probe passed.
- [ ] Add real guardian/tmux test exercising the separated pane and descendant stop; add invalid UID, failed drop and inaccessible workspace cases. Run RED.
- [ ] Implement sealed identity and drop, bootstrap preconditions, private workspace separation and user lookup. Keep production CLI's unavailable-transport refusal; expose the bootstrap and separated library APIs for B6a2 without adding its server transport.
- [ ] Run GREEN, existing sandbox runner tests and session-host tests. Commit.

### Task 3: Qualification and publication

**Files:** Create `docs/implementation/B6b.md`; update this plan.

**Interfaces:** consumes both implementations; produces named-test evidence and pushed branch.

- [ ] Run exact workspace filters `agent_user_cannot_read_permit_or_proxy_key` and `aq_proxy_injects_access_header_without_exposing_token`, require nonzero passing tests. Run formatter, Clippy and related suites with bounded resources.
- [ ] Record environment, precise commands/results, privilege prerequisites, production wiring limits, and commits in evidence.
- [ ] Obtain fresh whole-branch review, resolve material findings with regression tests.
- [ ] Squash unpublished leaf implementation history, merge configured integration source, run targeted checks if changed, guarded push and close with evidence. `origin/dev` is absent; supervisor clarification pending; never invent a branch.
