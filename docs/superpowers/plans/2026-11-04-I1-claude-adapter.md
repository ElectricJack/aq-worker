# I1 Claude Code Adapter Qualification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Qualify Claude Code 2.1.318 as an AQ harness. The C6a recipe must produce the native session transcript that the H1 reader consumes. Both Claude transcript readers must be qualified against a real 2.1.318 headless session. The live docker-local test must run the real recipe on the owner's subscription login.

**Architecture:** This plan builds on existing parts rather than adding a new adapter:
- C6a recipe: `native/adapter/claude.rs`
- C6b install: `runner/sandbox/subscription_login.rs`
- I4a live harness: `tests/claude_live.rs`
- 18.9.3 bundle pin: `packaging/sandbox/pins.json`

It closes three gaps:
1. **The recipe now writes the transcript.** The recipe disabled session persistence, so no transcript existed. It now pins `--session-id <attempt uuid>` and reports the exact native transcript path.
2. **The reader skips 2.1.318 metadata.** The reader turned every 2.1.318 metadata record into an `UnsupportedFormat` gap. It now skips the witnessed metadata types explicitly, and unknown types still gap.
3. **The live test runs the real recipe.** It renders the recipe's own argv, env and settings instead of hand-written flags. It then runs the reader over the real transcript it produced.

**Tech Stack:** Rust 2024, serde_json, uuid, aq-observation / aq-observation-engine, the docker-local backend (rootless Podman).

**Spec:** task azure-horizon-22.4. The contract is in aq-server `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml:4886`. 0005 §7 (Claude login: long-lived env-only token) and §8.3 (harness rows) apply. The owner decisions of 2026-11-04 (Claude subscription only, no API key; device-auth capture parked as steady-orbit-52) are in the aq-server supervision log.

## Global Constraints

- **Token handling.** The token never appears in argv, labels, create env, logs, evidence or committed fixtures. It reaches the agent only as `CLAUDE_CODE_OAUTH_TOKEN` in the spawned env (C6b `InstalledLogin::claude_env`).
- **Recipe environment.** The recipe env still contains no `FORBIDDEN_ENVIRONMENT` variable.
- **Confinement on this host.** Docker-local offers `container` confinement only.
- **Root deviation.** Claude refuses `bypassPermissions` as root unless `IS_SANDBOX=1` is set (witnessed on 2.1.318). The single-UID Podman on this host runs the runner as in-container root (see I4a's deviation), so the live test sets `IS_SANDBOX=1` as a documented test-only deviation. The recipe never sets it: a production agent is a non-root user (SBX7).
- **Fixtures are sanitized.** No prompt text, tool content, cwd, git branch or request bodies. Only record types, native identifiers that have been replaced, and numeric usage are kept.
- **Out of scope:** Gemini, OpenCode, H1 runner linking, usage costing, 0005 edits (a docs task is filed instead), placement and C4.

## Pending owner decisions

These were asked in msg-0f87623ce7c240228f6019720c7c1a6a.

- **API-key route.** The choices are a B6b Anthropic Messages route or a recorded limitation. Until the owner answers, the credential profile stays subscription-only (C6a's `FORBIDDEN_ENVIRONMENT`), and I1.md records the limitation.
- **Enrollment.** The choices are `claude setup-token` capture in a sandbox or operator import of the existing token file. The default is operator import, with capture deferred to steady-orbit-52.

## Review Focus

- **Transcript path.** The predicted path `<home>/projects/<slug>/<session>.jsonl` must match what the pinned CLI writes. The live test checks the file exists at exactly that path.
- **Skip list.** It must not swallow records that carry usage or tool facts. Only the witnessed metadata types are skipped; `system` (which includes `compact_boundary` context loss) stays a gap.
- **Fixture sanitization.** The committed fixture must contain no content from the real session (checked by test).

---

### Task 1: Reader qualification against a real 2.1.318 session

**Files:**
- Modify: `crates/aq-observation-engine/src/formats/claude.rs`
- Modify: `crates/aq-observation/src/formats/claude.rs`. It stays byte-identical to the engine copy.
- Create: `crates/aq-worker/tests/fixtures/claude-real-2.1.318.jsonl`. This is the sanitized live-run session.
- Create: `docs/implementation/I1-evidence/sanitize.py` and `provenance.json`.
- Modify: `crates/aq-worker/src/native/runner/sandbox/observations_tests.rs`. It gains a test mirroring H1's codex qualification.
- Modify: the `source-formats.json` Claude row in `crates/aq-observation/artifacts/` and its WRK-15 mirror.

- [ ] **Step 1.** Capture a real 2.1.318 headless session and sanitize it into the fixture.
- [ ] **Step 2 (RED).** Write the test `claude_real_session_2_1_318_qualifies_reference_and_durable_engine`. It feeds the fixture through the reference normalizer and the durable collector and asserts all of the following:
  - no gap;
  - one usage fact per `message.id`, with the native counters;
  - ToolStarted and ToolResult for each tool call;
  - one TurnBoundary;
  - the engine and the reference agree.

  It fails today because the 2.1.318 metadata records become `unsupported-claude` gaps.
- [ ] **Step 3 (GREEN).** Skip the witnessed metadata types in both readers:
  - headless: `atis-latch`, `api-request-shape`, `api-request-blob`, `api-request`, `tools-used`, `last-prompt` and `cost-state`;
  - interactive UI state: `mode`, `permission-mode`, `session-owner`, `ai-title`, `pr-link`, `file-history-snapshot` and `file-history-delta`.

  Keep `system` and unknown types as gaps.
- [ ] **Step 4.** Regenerate the artifacts with `cargo run -p aq-observation --example export` and the engine equivalent. Then run `-p aq-observation --test artifacts --test normalizers` and the engine tests.

### Task 2: Recipe writes the native transcript

**Files:**
- Modify: `crates/aq-worker/src/native/adapter/claude.rs` and `claude/tests.rs`
- Modify: `crates/aq-worker/src/native/runner/sandbox/harness_config.rs`, which gains `ClaudeAgentHome::plan`
- Modify: callers `native_probe.rs` and `tests/sandbox_security.rs`

- [ ] **Step 1 (RED).** Write the unit test `claude_recipe_pins_session_and_reports_native_transcript`. It asserts:
  - `--session-id <uuid>` is passed and `--no-session-persistence` is absent;
  - `recipe.transcript == <home>/projects/<slug(workspace)>/<uuid>.jsonl`, where the slug replaces every non-alphanumeric byte with `-`;
  - a workspace whose slug exceeds 200 bytes is refused.
- [ ] **Step 2 (GREEN).** Add `session: Uuid` to `ClaudePaths` and `transcript: PathBuf` to `ClaudeRecipe`. `ClaudeAgentHome::plan` returns the recipe without writing it, and `install` = `plan` plus the write.
- [ ] **Step 3.** Run `cargo test -p aq-worker --lib adapter::claude native_probe` and `--test sandbox_security`.

### Task 3: Live test runs the real recipe and qualifies the reader

**Files:**
- Modify: `crates/aq-worker/tests/claude_live.rs`

- [ ] **Step 1.** Plan the container-mode recipe through `authorize` and `ClaudeAgentHome::plan`, using in-container paths. Render settings, env and argv (POSIX-quoted) into the runner. The prompt arrives on stdin, never through argv or the shell. The runner adds the test-only `IS_SANDBOX=1` and passes `--max-turns`. The token comes from the material, as before.
- [ ] **Step 2.** Copy the native transcript from `recipe.transcript` to `out/session.jsonl`. Assert it exists, then run the reference normalizer over it and assert:
  - no `unsupported` gap;
  - usage is present;
  - the Edit tool was started and returned a result.
- [ ] **Step 3.** Run the offline tests (`cargo test -p aq-worker --test claude_live`). Then run the live test with `-- --ignored`, keeping the evidence in `docs/implementation/I1-evidence/run-*`.

### Task 4: Evidence

- [ ] Write `docs/implementation/I1.md`. It covers:
  - the §8.3 rows, with doc URLs and version 2.1.318;
  - the H2 minimum-contract table, with optional capabilities explicit;
  - template status (18.9.3 bundle pin; the advertised version path in `template_tool.py`);
  - commands and logs;
  - limits and owner decisions.
- [ ] File a docs task for folding the Claude rows into 0005 §8.3.
