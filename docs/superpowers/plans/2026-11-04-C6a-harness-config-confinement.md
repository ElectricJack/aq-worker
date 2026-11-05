# C6a Harness Configuration and Confinement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:executing-plans to carry out this plan task by task.

**Goal:** Configure the Claude and Codex harnesses inside the sandbox so no provider API key reaches the `agent` user. Declare each attempt's confinement mode (`native` or `container`) in the frozen launch manifest. Admit `container` only under the SBX19 conditions, and record qualification per template × backend.

**Architecture.** This follows the B7 pattern: library APIs plus tests, not wired into `run_as_agent`, because B6a2 owns that wiring.

- **Protocol.** The confinement signal is a capability. `aq.harness.confined-worktree` means native, and the new `aq.harness.container-confinement` means container. The manifest must declare exactly one.
- **Worker `sandbox/confinement.rs`.**
  - Derives the mode from the manifest.
  - Checks it against a worker-side mirror of the execution-profile permission and a qualification record (docker-local is container-only, per C2).
  - Checks runner/agent separation and that credentials are attempt-scoped.
  - Returns an unforgeable `AuthorizedConfinement`.
- **Worker `adapter/claude.rs`.** A pure Claude Code recipe: argv, env and settings.json. Native mode uses Claude's sandbox and `acceptEdits`. Container mode uses `bypassPermissions` with Claude's sandbox off. The recipe never carries an API key (subscription login only).
- **Codex mapper.**
  - `map`/`map_interactive` stay native-only and refuse the container capability.
  - A new `map_container` entrypoint emits `danger-full-access`.
  - `proxy_provider_overrides` defines `model_providers.aq-runner-proxy` (wire_api=responses) and sets `cli_auth_credentials_store="file"`.
- **Worker `sandbox/harness_config.rs`.** Builds the per-attempt private agent home for both harnesses, from an `AuthorizedConfinement` and the proxy capability.

**Tech stack.** Rust 2024; crates aq-session-protocol, aq-codex-mapper and aq-worker.

---

### Task 1: Protocol confinement capability

**Files:** `crates/aq-session-protocol/src/lib.rs`, `examples/export.rs`, `artifacts/fixtures.json`, `protocol/session-v1/fixtures.json`, `tests/conformance.rs`.

- [ ] Write failing tests in conformance.rs:
  - the fixture `container_manifest` validates;
  - `invalid_manifests` gains `both_confinements` and `no_confinement`, and both must fail;
  - `require_capabilities` accepts `aq.harness.container-confinement` v1.
- [ ] Implement:
  - add `CONTAINER_CONFINEMENT_CAPABILITY` and `NATIVE_CONFINEMENT_CAPABILITY` consts;
  - extend the allowlist;
  - `LaunchManifest::validate` requires startup-context and stop, plus exactly one confinement capability.
- [ ] Regenerate fixtures: `cargo run --locked -p aq-session-protocol --example export`.
- [ ] Run `cargo test --locked -p aq-session-protocol` until it passes.

### Task 2: Codex mapper modes and proxy provider

**Files:** `crates/aq-codex-mapper/src/lib.rs`, `src/tests.rs`.

- [ ] Write failing tests:
  - `map` refuses the container capability;
  - `map_container` requires it and emits `--sandbox danger-full-access` while keeping every other hardening flag identical;
  - `proxy_provider_overrides` emits `model_provider="aq-runner-proxy"`, `base_url=http://127.0.0.1:P/v1`, `wire_api="responses"`, `requires_openai_auth=true`, `cli_auth_credentials_store="file"`, and no `openai_base_url`.
- [ ] Implement, then run `cargo test --locked -p aq-codex-mapper`.

### Task 3: Confinement policy and qualification records (SBX7/13/19)

**Files:** `crates/aq-worker/src/native/runner/sandbox/confinement.rs` (new) and `sandbox.rs` (mod).

- [ ] Write the failing test `container_mode_requires_profile_permission`. Container must be refused when:
  - the profile doesn't permit it;
  - the backend provides no boundary;
  - the agent uid is root or the same as the runner uid;
  - the credentials are not attempt-scoped.

  It is admitted when every condition holds.
- [ ] Write the failing test `native_required_profile_refused_on_container_only_backend`. It also checks that native is never downgraded to container (SBX13) and that docker-local defaults to container.
- [ ] Implement `ConfinementMode::from_manifest`, `ExecutionConfinement`, `QualificationRecord` (with `docker_local`), `AttemptSeparation` and `authorize`.

### Task 4: Claude Code recipe (owner decision: Claude first)

**Files:** `crates/aq-worker/src/native/adapter/claude.rs` (new) and `adapter.rs` (mod).

- [ ] Write failing tests:
  - native recipe: `-p`, stream-json, `--permission-mode acceptEdits`, settings with `sandbox.enabled=true` and `allowUnsandboxedCommands=false`, bypass disabled;
  - container recipe: `--permission-mode bypassPermissions` and `sandbox.enabled=false`;
  - env holds only HOME, CLAUDE_CONFIG_DIR, PATH and the non-essential-traffic switches, with no `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN`;
  - the recipe refuses a non-claude harness, a non-anthropic provider, relative paths, and a private home inside the workspace.
- [ ] Implement.

### Task 5: Agent home builder and key isolation

**Files:** `crates/aq-worker/src/native/runner/sandbox/harness_config.rs` (new), `crates/aq-worker/tests/sandbox_security.rs`.

- [ ] Write the failing test `api_key_never_visible_to_agent_user`.
  - Setup: put the provider key in a runner-private file and start the `RunnerResponsesProxy` development loopback.
  - Build the Codex agent home (authorized native) and the Claude agent home.
  - Assert the key bytes are absent from the env, the argv and every file under each private home.
  - Assert no `OPENAI_API_KEY`/`CODEX_API_KEY`/`ANTHROPIC_API_KEY` env entries.
  - Assert the Codex auth.json holds only the local capability.
- [ ] Implement `CodexAgentHome::install` and `ClaudeAgentHome::install`. Each creates a private 0700 home, writes owner-only files and returns the recipe.

### Task 6: Evidence and follow-ups

- [ ] Write `docs/implementation/C6a.md` plus `C6a-evidence/` (test logs).
- [ ] Post `CROSS-REPO FOLLOW-UP (aq-server):` for the `aq-profiles` `Execution` confinement permission and the server emitting the confinement capability.
- [ ] File the direct follow-ups:
  - wire the builders into B6a2's launch;
  - migrate the native scoped path from `openai_base_url` to model_providers;
  - add a Claude packaging pin;
  - run a live native-mode probe per template × backend.
