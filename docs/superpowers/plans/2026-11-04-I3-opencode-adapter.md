# I3 — OpenCode adapter: implementation plan

Task `azure-horizon-22.6`. Goal: qualify OpenCode as an AQ sandbox harness (H2 minimum contract), with its transcript reader and, because OpenCode supports one, the file-stored ChatGPT subscription login.

## Findings that shape the plan (OpenCode 1.18.35, host install)

- Headless: `opencode run --format json` emits JSONL events (`step_start`, `tool_use`, `text`, `step_finish`).
- Config: `OPENCODE_CONFIG` (file), `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_DISABLE_AUTOUPDATE`; data under `$XDG_DATA_HOME/opencode`.
- Credentials: `$XDG_DATA_HOME/opencode/auth.json`, `{provider: {type:"oauth",refresh,access,expires,accountId?} | {type:"api",key}}`. OpenAI offers "ChatGPT Pro/Plus (headless)" device login, refreshed in place.
- The reader's v2 `Session.Message` DTO (`/api/session/:id/message`) is empty for `opencode run` sessions in 1.18.35; real sessions surface as `{info, parts}` records (`/session/:id/message`, `opencode export`).
- No OS sandbox: `container` confinement only.

## Steps (test-first)

1. **Reader** (`aq-observation{,-engine}/src/formats/opencode.rs`, cursor in `normalize.rs`): accept `{info, parts}` records; fixture from a real 1.18.35 session; tests for usage, tool start/result, text roles, turn boundary, ignored step parts.
2. **Adapter** `crates/aq-worker/src/native/adapter/opencode.rs`: pure recipe (argv, env, `opencode.json`), pinned `1.18.35`, `openai` provider, two routes (`ResponsesProxy{origin}`, `Subscription`), refuse native confinement and forbidden env. Unit tests.
3. **Home** `harness_config.rs::OpenCodeAgentHome`: private 0700 home; proxy route writes `auth.json` with only the proxy capability.
4. **Subscription** `subscription_login.rs`: `LoginKind::OpenCodeChatgptAuth`, install into the private data dir, write-back when `openai.expires` advances, wipe. Tests mirror the Codex ones.
5. **Enrollment**: `scripts/opencode-enroll.py` runs `opencode auth login` into a fresh private dir and emits a validated 0600 material file for the C5b upload (server side is aq-server).
6. **Template**: pin `opencode-linux-x64@1.18.35` in `packaging/sandbox/pins.json`, packager, Dockerfile, manifest, credential scan, `runtime_bundle.rs`.
7. **Live test** `crates/aq-worker/tests/opencode_live.rs` (`#[ignore]`), I4a shape: derived image, controller lifecycle, edit/check/publish, transcript normalized by the reader, credential wiped.
8. **Evidence** `docs/implementation/I3.md`: §8.3 rows, H2 table, provider choice, limits.
