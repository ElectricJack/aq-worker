# I4a — Claude Code CLI qualification in docker-local (re-scoped from Codex)

Task: `azure-horizon-22.2`. Plan: commit `2000ffa` (`docs/superpowers/plans/2026-11-04-I4a-claude-live.md`). Implementation and evidence: one squashed commit, the one containing this document. The exact published OID is recorded in the AQ task close.

The owner re-scoped this task on 2026-11-04 from Codex (ChatGPT login) to the Claude Code CLI with the owner's Claude subscription login. Codex qualification moves to a later follow-up. The original deliverable filter, `--test codex_live chatgpt -- --ignored`, is obsolete and is declared unmet at close; the replacement is `--test claude_live -- --ignored`.

## Decisions

- **The image is derived and unadmitted.** `I4a-evidence/Containerfile.claude-rust` layers the CLI on the C3c `rust-1` template, which is itself unadmitted on this host because the C3c template tool's scanner refuses here (GnuTLS). The test allowlists the image in its own `DockerLocalConfig`; no shared template allowlist is changed.
- **The CLI is pinned three ways.** The npm package version (`@anthropic-ai/claude-code@2.1.318`, the native executable) is pinned at build time and checked with `claude --version` in the build. The image is pinned by manifest digest. The controller's reservation matches the manifest's harness `claude-code` and `cli_version 2.1.318` against the node's template advertisement. `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` are set in the image.
- **Auth is the long-lived token, leased as private material.** The test's `Lease` permit provider reads the owner's 0600 `claude setup-token` file (refusing group or world access) into a zeroizing buffer and returns it as the private payload of a preparation permit bound to the attempt, node and boot. The controller delivers it to `/aq/private/material`. The runner moves it into `CLAUDE_CODE_OAUTH_TOKEN`, deletes the file before any work starts, and unsets the variable before writing its result. The token never appears in argv, container env, labels or logs.
- **No refresh and no write-back.** A `setup-token` token is long-lived; Claude Code does not refresh it and, with only the env token, writes no `~/.claude/.credentials.json`. The test asserts that file is absent in the sandbox and that the host's own `~/.claude/.credentials.json` (size and mtime) is unchanged, so there is no shared refresh lineage with the host login.
- **The runner is in-container uid 0, not a non-root user (deviation).** The owner asked for a non-root user. On this host the dedicated sandbox Podman runs in single-UID mode (`AQ_SANDBOX_SINGLE_UID=1`, uid_map `0 1000 1`, no `newuidmap`), so no other UID can start under the backend's fixed create policy:
  - `--user 10002` fails in runc: `chown /dev/stdout: invalid argument`;
  - a nested `unshare --user --map-user=10002` fails with EPERM unless the container is `--privileged`.

  The runner therefore runs as in-container uid 0, which is host uid 1000, with `CapEff 0000000000000000` and `NoNewPrivs 1` (both asserted). Claude Code's root refusal applies only to the bypass-permissions modes; the runner uses `--permission-mode acceptEdits --allowedTools Read,Edit` and never `--dangerously-skip-permissions` (asserted on the runner text). Owner fix: install `uidmap`, restart the sandbox Podman without `AQ_SANDBOX_SINGLE_UID`, and add `USER agent` (10002) to the image.
- **Teardown runs before any outcome assertion.** A failing run still stops and destroys its sandbox, so the RED run left nothing behind.

## Delivered

### Image (`docs/implementation/I4a-evidence/Containerfile.claude-rust`)

| Item | Value |
| --- | --- |
| Base | `127.0.0.1:5138/electricjack/aq-template-unadmitted@sha256:4c9c20007445c3a207f31787e635ec753e64b13f22300cd90030784f7862a1c5` (C3c `rust-1`) |
| CLI | Claude Code `2.1.318` at `/opt/aq/bin/claude` (`/opt/aq/harnesses/claude-code`) |
| Image | `127.0.0.1:5138/electricjack/aq-template-unadmitted@sha256:194e11f54fbe104734166ec219f5ac13b2e54055fae01f6c3ce264a915f172a4` (tag `claude-rust-1`) |
| Local image ID | `45937ad6e4ee283342774d758823e49de1bd61080c874a245b73c3e59565721b` |
| Model (subscription default) | `claude-sonnet-5-5` |

Commands, with `podman` the bundled sandbox binary and `--url unix:///run/user/1000/aq-sandbox/podman.sock`:

```
podman build --tls-verify=false -f Containerfile.claude-rust -t localhost/aq-i4a-claude-rust:2.1.318 .
podman push --tls-verify=false --digestfile digest localhost/aq-i4a-claude-rust:2.1.318 \
  127.0.0.1:5138/electricjack/aq-template-unadmitted:claude-rust-1
podman pull --tls-verify=false 127.0.0.1:5138/electricjack/aq-template-unadmitted@sha256:194e11f5…172a4
```

`DockerLocalBackend::pull_template` then trusts only the daemon's inspect of that digest.

### Live test (`crates/aq-worker/tests/claude_live.rs`)

- `Controller<Shared, Lease>` drives the real `DockerLocalBackend`. `Shared` wraps the backend in `Arc<tokio::Mutex<_>>` and delegates `PlacementBackend`, so the test can observe `runner_exit` and `read_file` on the sandbox the controller owns.
- Controller path: `open` → `register` → `reserve` (harness, CLI version, template digest) → `accept_with` → `launch` (create, deliver material, start runner) → `stop` (stopped-writer proof) → `destroy` (`operator_released`).
- The fixed runner (`RUNNER`, public configuration with no secret):
  1. reads and deletes the material;
  2. creates a bare publish repo in the work volume and a crate whose `add` returns `a - b`;
  3. commits a baseline and confirms `cargo test --offline` fails (RED);
  4. runs `claude -p` (acceptEdits, `Read,Edit` only, 12 turns max, stream-json) to fix `src/lib.rs`;
  5. runs `cargo test --offline` (the check);
  6. commits and pushes to the bare repo (the publish);
  7. scans `$HOME`, `/aq` and `/tmp` for the token with the pattern read from a pipe, never argv;
  8. writes `out/result.json` and exits 0 only if Claude, the check and the publish all succeeded.
- Pure tests: `docker_local_offers_container_confinement_only` and `runner_carries_no_secret_and_wipes_the_material`.

## Acceptance map

- **`live_claude_subscription_edit_check_publish_cycle`** (ignored, live).

  | Check | Expected | Observed (runs 1–3) |
  | --- | --- | --- |
  | Runner exit | `Exited(0)` | `Exited(0)` after ~15 s each |
  | CLI version in the sandbox | `2.1.318 (Claude Code)` | equal |
  | Capabilities / no-new-privs | `CapEff 0`, `NoNewPrivs 1` | equal; `uid 0` recorded |
  | Material | deleted by the runner; unreadable afterwards | `material_wiped true`, read fails |
  | Baseline | `cargo test` fails before the edit | `red true` |
  | Edit | only `src/lib.rs` changed, now `a + b`; Claude used `Edit` and reported `is_error false` | equal |
  | Check / publish | `check_rc 0`, `publish_rc 0`, published oid == head ≠ base | equal |
  | Token on disk | 0 files under `$HOME`, `/aq`, `/tmp`; absent from every read-back file | 0 |
  | Credential write-back | no `.credentials.json` in the sandbox; host login size and mtime unchanged | equal (host file exists, so the comparison is real) |
  | Teardown | stop confirmed with the volume preserved, destroy confirmed, `list_owned` empty | equal; daemon shows 0 containers and 0 volumes afterwards |

- **0005 §11 "Harness confinement", rust × docker-local:** `container` only. `docker_local_offers_container_confinement_only` asserts `require_confinement(Container)` is accepted and `Native` is refused.

### H2 minimum-contract rows (0003 §4) for Claude Code on this path

| Row | Status here |
| --- | --- |
| Version and config validation | **Covered.** Build-time version check, in-sandbox version assertion, and controller reservation by harness name, CLI version and template digest. |
| Protected credential binding | **Covered for the test lease.** Permit bound to attempt, node and boot; payload only as private material; wiped; no leak; no write-back. The production credential lease service is C6b (not merged). |
| Scoped AQ client and initial context | **Not covered.** Needs the C6a runner config with B7's scoped `aq-agent`; this runner gives Claude a prompt only. |
| Attempt-owned tmux launch | **Not covered.** The runner runs `claude -p` headless under `bash -c`; tmux launch belongs to the C6a runner. |
| Observed process outcome | **Covered.** `runner_exit` plus the stream-json `result` (`success`, `is_error false`). |
| Tested stop | **Covered.** Controller stop with a stopped-writer proof, then destroy. |
| Worktree confinement | **Covered at the container level.** Container-only confinement, `Read,Edit` tool allowlist, and only `src/lib.rs` changed. |
| Refusal of unsupported grants | **Partly covered.** `Native` confinement is refused; harness-level grant refusal is C6a. |
| Remote watch and control | **Not covered.** Exercised by the MS1 demo end to end. |
| Optional: resume | Not exercised. |
| Optional: native message injection | Not exercised (F3 uses the inbox for harnesses without a qualified safe boundary). |
| Optional: usage | Observed, not wired: stream-json reports `modelUsage` and `total_cost_usd` (about $0.03 per run, list cost basis). |

## Verification

- **Environment:** Linux 6.18.33.2-microsoft-standard-WSL2; rootless Podman 5.8.8 sandbox daemon at `/run/user/1000/aq-sandbox/podman.sock` with `AQ_DOCKER_LOCAL_SHARED_ROOT_FS=1`.
- **Cargo settings:** the committed lockfile, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2` (runs 2 and 3 through `docs/implementation/C1-evidence/test_cargo_check.py`).
- **Live env:** `AQ_DOCKER_LOCAL_SOCKET`, `AQ_CLAUDE_LIVE_IMAGE` (the digest above), `AQ_CLAUDE_LIVE_TOKEN_FILE` (the owner's token file), `AQ_CLAUDE_LIVE_EVIDENCE_DIR`.

**RED:** with the runner stubbed to `exit 3`, the live test failed on `assert_eq!(exit, Some(RunnerExit::Exited(0)))` with `Exited(3)`, after the teardown assertions passed (`I4a-evidence/red.log`).

**GREEN:** three consecutive passes. Runs 1 and 2 were before a formatting-only `rustfmt` pass; run 3 was after it.

| Exact command | Result | Evidence in `I4a-evidence/` |
| --- | --- | --- |
| `cargo test --locked -p aq-worker --test claude_live live -- --ignored --nocapture` (run 1) | 1 passed | `live-run-1.log`, `run-1/` |
| same, through the C1 wrapper (run 2) | 1 passed | `live-run-2.log`, `run-2/` |
| same, through the C1 wrapper (run 3) | 1 passed | `live-run-3.log`, `run-3/` |
| `cargo test --locked -p aq-worker --test claude_live` | 2 passed, 1 ignored | `test.log` |
| `cargo clippy --locked -p aq-worker --test claude_live -- -D warnings` | clean | `lint.log` |
| `rustfmt --edition 2024 --check crates/aq-worker/tests/claude_live.rs` | clean | `lint.log` |

Each `run-N/` holds the read-back sandbox outputs: `result.json`, `claude.jsonl` (stream-json transcript), `red.log`, `check.log`, `changed.txt`, `published.txt`, `publish.log`, `claude.err` and the fixed `lib.rs`. A search of `I4a-evidence/` and the test source for the token finds nothing.

## Limits and follow-ups (no children filed, per the owner)

- **Non-root user:** not possible under this host's single-UID sandbox Podman; owner fix above.
- **C6a / C6b:** not merged at close (`calm-delta-18.9` in progress, `calm-delta-18.10` ready), so the runner confinement mode and the credential lease service are not exercised; the MS1 demo covers that path.
- **Template admission:** the derived image, like `rust-1`, is unadmitted until the C3c scanner works on this host.
- **Codex:** qualification of Codex CLI 0.166.0 with the ChatGPT login (the original scope, including `auth.json` write-back) moves to a later follow-up once Codex usage resets (about Nov 8).
- **H2 rows not covered** above (scoped AQ client, tmux launch, harness-level grant refusal, remote watch and control) depend on C6a.
