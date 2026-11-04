# C6a — Harness configuration and confinement modes

Task: `calm-delta-18.9`. Plan: commit `8f2f5be` (`docs/superpowers/plans/2026-11-04-C6a-harness-config-confinement.md`). Implementation and evidence: one squashed commit, the one containing this document. The exact published OID is recorded in the AQ task close.

## Decision: Claude first

The owner's decision of 2026-11-04 put Claude Code on MS1's critical path. The runner must configure the `claude` harness for unattended runs: confinement modes, settings and permission flags, workdir, and no API key (subscription login only). The Codex configuration in the task's original scope follows, but it is off the critical path. C6a delivers both, with Claude done first.

The confinement mode is a **manifest capability**, so it is frozen with the launch manifest and never chosen at run time:

| Mode | Capability | Meaning |
| --- | --- | --- |
| `native` | `aq.harness.confined-worktree` (existing) | The harness's own sandbox runs inside our sandbox |
| `container` | `aq.harness.container-confinement` (new) | The harness sandbox is off; the backend sandbox is the boundary (0005 SBX19) |

A manifest must declare exactly one. The existing native-only Codex path (`map`, `map_interactive`, scoped `openai_base_url`) is unchanged, except that it now refuses a container manifest.

## Delivered

### Protocol (`crates/aq-session-protocol`)

- **`src/lib.rs:19-22`** adds `NATIVE_CONFINEMENT_CAPABILITY` and `CONTAINER_CONFINEMENT_CAPABILITY`. It also extends the capability allowlist (line 166).
- **`LaunchManifest::validate`** requires exactly one confinement capability (lines 298-299).
- **Fixtures:** `examples/export.rs` emits a valid `container_manifest`, plus two invalid manifests, `both_confinements` and `no_confinement`. Both fixture files are regenerated: `artifacts/fixtures.json` and `protocol/session-v1/fixtures.json`.

### Confinement policy (`crates/aq-worker/src/native/runner/sandbox/confinement.rs`)

- **`ConfinementMode::declared(manifest)` (line 23)** validates the manifest and reads the mode from it.
- **`ExecutionConfinement { permit_container, require_native }`** is the worker-side mirror of the execution profile's permission. Its default is deny.
- **`QualificationRecord`** is one record per template × backend: its boundary, `native_supported`, `default_mode` and evidence.
  - `from_native_probe` qualifies native only when a write outside the workspace was refused.
  - `docker_local` (line 111) is container-only, with `default_mode = container`, citing C2's `unshare -Ur id` EPERM.
- **`admit_profile` (line 154)** refuses a profile that requires native on a container-only backend. It also refuses a backend where native is unqualified and the profile does not permit container.
- **`authorize` (line 169)** returns an `AuthorizedConfinement`. Its fields are private, so code outside the module can obtain one only from `authorize`. Rules by declared mode:
  - **`native`** needs a qualified native record. There is no fallback to container (SBX13).
  - **`container`** needs all four of the following:
    - explicit profile permission (and no `require_native`);
    - a backend-provided boundary;
    - a distinct, unprivileged agent user (uid ≠ 0, gid ≠ 0, uid ≠ runner uid; SBX7);
    - attempt-scoped credentials.

### Claude Code recipe (`crates/aq-worker/src/native/adapter/claude.rs`)

**`map(harness, model, paths, mode)` (line 65)** is a crate-private pure recipe; other crates reach it only through `ClaudeAgentHome::install`. It returns argv, environment, working directory and the `settings.json` bytes. It refuses:

- a non-`claude` harness, a CLI version other than `2.1.318`, a non-`anthropic` provider, or a spending budget;
- a mode that differs from the one the manifest declares;
- non-absolute or non-normalized paths, or paths containing NUL;
- a private home that overlaps the workspace in either direction.

**Argv:**

```
-p --output-format stream-json --verbose --input-format text --model <m>
--permission-mode <acceptEdits|bypassPermissions>
--setting-sources user --settings <home>/settings.json
--strict-mcp-config --no-session-persistence --disable-slash-commands
```

`--setting-sources user` means Claude never loads the workspace's project or local settings, which repository content controls.

**Workdir:** the process working directory is the workspace. Claude has no `--cwd` flag.

**Environment:** exactly these variables:

- `HOME` and `CLAUDE_CONFIG_DIR`, both set to the private home;
- a fixed `PATH`;
- `DISABLE_AUTOUPDATER`, `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` and `CLAUDE_CODE_DISABLE_AUTO_MEMORY`, each set to `1`.

`FORBIDDEN_ENVIRONMENT` lists the variables that would replace the subscription login, and the recipe never sets them: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, the Bedrock/Vertex/Foundry switches, `ANTHROPIC_BASE_URL` (a redirected base URL would send the subscription login to another host), and the OpenAI/Codex keys.

**Settings (`settings()`, line 166):**

| Mode | `permissions` | `sandbox` |
| --- | --- | --- |
| `native` | `defaultMode: acceptEdits`, `disableBypassPermissionsMode: "disable"` | `enabled: true`, `autoAllowBashIfSandboxed: true`, `allowUnsandboxedCommands: false` |
| `container` | `defaultMode: bypassPermissions` | `enabled: false` |

Both modes also set `disableAllHooks: true`, `enableAllProjectMcpServers: false`, `cleanupPeriodDays: 1` and `forceLoginMethod: "claudeai"`.

### Codex mapper (`crates/aq-codex-mapper/src/lib.rs`)

- **`map` and `map_interactive`** stay native-only (`--sandbox workspace-write`). They refuse a manifest that declares container confinement.
- **`map_container` (line 94)** emits `--sandbox danger-full-access`. It requires the declared container capability, and every other hardening flag is identical to `map`.
- **`proxy_provider_overrides(endpoint)` (line 250)** emits:

  ```
  model_provider="aq-runner-proxy"
  model_providers.aq-runner-proxy.{name, base_url="http://127.0.0.1:<port>/v1", wire_api="responses", requires_openai_auth=true}
  cli_auth_credentials_store="file"
  ```

  It also emits the Responses transport switches (no websockets, analytics or otel). It emits no `openai_base_url`, no `env_key` and no bearer.

### Private agent homes (`crates/aq-worker/src/native/runner/sandbox/harness_config.rs`)

Both builders take an `&AuthorizedConfinement`, so neither can configure an unauthorized mode. Each one:

- creates a fresh 0700 home owned by the effective uid, and refuses an existing path;
- writes files with `create_new`, mode 0600 and `O_NOFOLLOW`.

**`CodexAgentHome::install` (line 58):**

- It refuses a private home that overlaps the workspace in either direction, because native `workspace-write` would otherwise let the agent rewrite its own `CODEX_HOME`.
- It accepts only the proxy's attempt-local capability (`aq-responses-<64 hex>`, 77 bytes). Any other bytes, including a provider key, are refused.
- It maps by mode and splices `--config <proxy_provider_overrides>` in before the trailing stdin marker `-`.
- It writes `auth.json` as `{"OPENAI_API_KEY":"<capability>"}`, using `Zeroizing` buffers.
- The provider key stays in the runner-private file read by `RunnerResponsesProxy`.

**`ClaudeAgentHome::install` (line 106):** runs `claude::map` and writes only `settings.json`. There is no key, token, `apiKeyHelper` or `env` block.

## Acceptance map

- **t1 — `crates/aq-worker/src/native/runner/sandbox/confinement_tests.rs:61`, `container_mode_requires_profile_permission`.** A container manifest on a container-boundary backend is checked against these cases:

  | Case | Expected result |
  | --- | --- |
  | Default (deny) profile | Refused |
  | `permit_container` together with `require_native` | Refused |
  | Backend with no boundary | Refused |
  | Agent uid 0 | Refused |
  | Agent gid 0 | Refused |
  | Agent uid equal to the runner uid | Refused |
  | Credentials not attempt-scoped | Refused |
  | Every condition holds | Authorized as `container` |

- **t2 — `crates/aq-worker/tests/sandbox_security.rs:360`, `api_key_never_visible_to_agent_user`.**
  - **Setup:** the provider key is written to a runner-private `provider.key`, and the real B6b `RunnerResponsesProxy::development_loopback` reads it.
  - **Modes:** the test covers both native (VM-qualified) and container (`docker_local`) authorizations. For each mode it builds the Codex home from `proxy.origin()` and `local_capability()`, and also the Claude home.
  - **Assertions:**
    - no `FORBIDDEN_ENVIRONMENT` variable is set;
    - neither the key bytes nor the `provider.key` path appear in argv or the environment;
    - every home is 0700 and every file in it is 0600, and no file contains the key bytes;
    - the Codex environment is exactly `CODEX_HOME`;
    - the Codex argv routes through `model_provider="aq-runner-proxy"` with `cli_auth_credentials_store="file"` and no `openai_base_url=`;
    - `auth.json` equals the capability JSON;
    - passing the key itself as the capability is refused;
    - a Codex home nested inside the workspace is refused and not created;
    - the Claude home holds only `settings.json`, with no `apiKeyHelper` and no `env` block;
    - the runner's key file is unchanged.
- **Container mode is chosen only from the frozen manifest.** Covered by `confinement_tests.rs:44` (`declared_mode_is_read_from_the_frozen_manifest`) and `crates/aq-session-protocol/tests/conformance.rs:386` (`manifest_declares_exactly_one_confinement_mode`).
- **No silent fallback; docker-local defaults to container; native-required profile refused on a container-only backend.** Covered by `confinement_tests.rs:105` (`native_required_profile_refused_on_container_only_backend`).
- **Codex modes keep the hardening flags; proxy provider without a key.** Covered by `crates/aq-codex-mapper/src/tests.rs:199` (`confinement_mode_selects_codex_sandbox_and_keeps_hardening`) and `:226` (`proxy_provider_routes_codex_through_runner_loopback_without_key`).
- **Claude configuration.** `crates/aq-worker/src/native/adapter/claude/tests.rs` has four tests: native (line 50), container (line 93), subscription-only environment (line 112) and refusals (line 144).

## Verification

- **Environment:** Linux 6.18.33.2-microsoft-standard-WSL2.
- **Cargo settings:** the committed lockfile, `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`.
- **Source tree:** the logs were captured on base `8f2f5be` plus the uncommitted C6a tree. That tree has the same content as the squashed commit, minus this document.

| Exact command | Result | Evidence in `C6a-evidence/` |
| --- | --- | --- |
| `cargo test --locked --workspace container_mode_requires_profile_permission` | 1 matching test passed | `container_mode_requires_profile_permission.log` |
| `cargo test --locked --workspace api_key_never_visible_to_agent_user` | 1 matching test passed | `api_key_never_visible_to_agent_user.log` |
| `cargo test --locked -p aq-session-protocol -p aq-codex-mapper` | 20 + 11 passed | `protocol-and-mapper.log` |
| `cargo test --locked -p aq-worker --lib -- claude confinement` | 7 passed | `worker-claude-confinement.log` |
| `cargo test --locked -p aq-worker --test sandbox_security` | 9 passed | `sandbox_security.log` |
| `cargo clippy --locked -p aq-worker -p aq-codex-mapper -p aq-session-protocol --all-targets -- -D warnings` | passed | `clippy.log` |
| `rustfmt --edition 2024 --check <changed .rs files>` | passed | `fmt.log` |

`cargo fmt --check` cannot run in this nested worktree (see B7), so `rustfmt` is run directly on the changed files.

**RED:** the named tests were written against APIs that did not exist yet: `authorize`, `CodexAgentHome`, `ClaudeAgentHome`, `map_container` and `proxy_provider_overrides`. They failed to compile before the implementation. No RED log was kept.

## Adversarial review

An independent reviewer read the uncommitted tree against this task's acceptance criteria. It found no blocking defects. Dispositions:

| Finding | Severity | Disposition |
| --- | --- | --- |
| Codex private home could sit inside the workspace, where native `workspace-write` could rewrite `auth.json` | nit | **Fixed.** `CodexAgentHome::install` refuses overlap; t2 asserts it |
| `ANTHROPIC_BASE_URL` and `CLAUDE_CODE_USE_FOUNDRY` missing from `FORBIDDEN_ENVIRONMENT` | nit | **Fixed** |
| `claude::map` was public and needed no authorization | nit | **Fixed.** It is now `pub(crate)`; `aq_codex_mapper::map_container` stays public metadata, documented as requiring the caller's authorization |
| Agent gid 0 not tested | nit | **Fixed.** Added to t1 |
| SBX7 inputs are caller claims; runner gid is not compared | nit | Limit below; follow-up `calm-delta-18.9.1` (launch wiring supplies real ids) |
| Home created by path; `O_NOFOLLOW` protects only the final component | nit | Limit below; follow-up `calm-delta-18.9.1` |
| t2 runs as one uid, so it does not prove the agent uid cannot open `provider.key` | nit | Limit below; follow-up `calm-delta-18.9.4` |
| Claude native mode could run commands unsandboxed if bubblewrap is unavailable | follow-up | Limit below; follow-up `calm-delta-18.9.4` |
| Assertions use `is_err()` only; `both_confinements` fixture only checked for decode failure | nit | Accepted; t1's profile case checks the message |

## Limits

- **Not wired into launch.** These are library APIs with tests, following the B7 pattern. B6a2 owns:
  - calling `authorize` from the frozen manifest;
  - installing the homes through `run_as_agent`;
  - spawning the recipe.
- **Installation identity.** `create_private_home` requires that the effective uid owns the home. Production installation must therefore run as the agent user.
- **Existing native consumers still refuse container manifests.** `aq-session-host/src/host_plan.rs` (lines 106-118) and `aq-worker/src/native/runner/codex.rs` (line 109) accept only `aq.harness.confined-worktree`. Supporting container there is a follow-up.
- **Claude OAuth leasing is C6b.** The recipe and home carry no credential. Leasing the subscription token (no refresh write-back, wiped on teardown) is out of scope.
- **Profile permission is mirrored worker-side.** The authoritative `Execution` confinement permission belongs to aq-server's `aq-profiles`, and so does emitting the confinement capability. Both are cross-repo follow-ups. Until then, `ExecutionConfinement::default()` denies container.
- **Qualification is recorded, not probed live.** `docker_local` cites the C2 probe. A live outside-write probe per template × backend is a follow-up.
- **Claude version is checked against the manifest, not pinned in packaging.** Adding a packaging pin for `2.1.318` is a follow-up.
- **The native scoped Codex path still uses `openai_base_url`.** Migrating it to `model_providers` is a follow-up.
- **SBX7 inputs are caller claims.** `authorize` compares the uid/gid it is given; it does not call `geteuid` or compare the runner gid. The launch wiring (`calm-delta-18.9.1`) must pass the real runner and agent identities.
- **The home is created by path, not `mkdirat`/`openat`.** `O_NOFOLLOW` covers only the final component. The hardened pattern is `aq-session-host/src/private_inputs.rs` (around line 456); adopting it belongs with launch wiring.
- **t2 runs as a single uid.** It proves the key never enters argv, environment or the homes, not that the agent uid cannot open the runner's `provider.key`. A two-uid check belongs with the live probe (`calm-delta-18.9.4`).
- **Claude native fail-closed is unverified.** `allowUnsandboxedCommands: false` is set, but whether Claude refuses to run commands when bubblewrap is unavailable is not tested here (`calm-delta-18.9.4`).

## Follow-ups

- `calm-delta-18.9.1` — wire C6a harness homes into sandbox launch (`run_as_agent`).
- `calm-delta-18.9.2` — migrate the native scoped Codex path from `openai_base_url` to `model_providers`.
- `calm-delta-18.9.3` — pin Claude Code CLI `2.1.318` in sandbox packaging.
- `calm-delta-18.9.4` — live native-confinement probe per template × backend.
- `calm-delta-18.9.5` — accept container-confinement manifests in `host_plan` and runner `codex`.
- Cross-repo (aq-server), recorded as a comment on `calm-delta-18.9`: the `aq-profiles` `Execution` confinement permission, emitting exactly one confinement capability, and qualification-based scheduling.
