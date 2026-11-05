# I2 handoff: Gemini CLI 0.67.0 rows for architecture 0005 §8.3

Task: `azure-horizon-22.5.1` (docs follow-up of I2, `azure-horizon-22.5`).

This handoff is published in `aq-worker` for a separately authorized aq-server
applier, as with the [C2 handoff](C2-0005-patch.md). It does not edit the
canonical aq-server design, and it qualifies no configuration. The Gemini live
qualification against Google stays blocked on owner credentials (see I2).

## Target and baseline

Target: `aq-server/docs/architecture/0005-sandboxed-execution-and-deployment-topology.md`,
section 8.3 "Harness configuration inside sandboxes".

Baseline: aq-server `origin/main` at `24a36243f3be0975695ca76daa617eb6f6f6fa5b`,
fetched on 2026-11-04. The last commit touching the target is
`6502afb0d8dd5cbcb93b17d276db8ddb246672db`. Document SHA-256:
`978a0c5bce41f7524a1df4de1809755f97bfc76352c3fb46ef158344f85ec6ea`.

**The application gate is the document SHA-256, not the commit.** If the
target document has different bytes, check that the Before text below is still
present exactly once before applying. If it is not, this handoff is stale and
must be refreshed instead of merged by hand.

## Sources

- I2 rows: [`docs/implementation/I2.md`, "0005 §8.3 rows for Gemini CLI"](https://github.com/ElectricJack/aq-worker/blob/6c1936f3ab76bb3b7797f9f40588be81dbfef87a/docs/implementation/I2.md),
  at aq-worker `6c1936f3ab76bb3b7797f9f40588be81dbfef87a` (branch
  `aq/azure-horizon-22.5`). The rows below copy that table. They only change
  wording that referred to the I2 run itself ("this run", "follow-up filed").
- Vendor documentation at tag `v0.67.0`. All five doc URLs returned HTTP 200 on
  2026-11-04: `docs/cli/headless.md`, `docs/reference/configuration.md`,
  `docs/cli/enterprise.md`, `docs/get-started/authentication.mdx` and
  `docs/cli/sandbox.md`.
- System settings check, read at tag `v0.67.0` on 2026-11-04:
  [`packages/core/src/utils/security.ts`](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/packages/core/src/utils/security.ts)
  (SHA-256 `2b314e13784379d11bf6401a9f4d54f90655f8ecd63826ec8d9cb569cd98ffc5`):
  `checkPosixStatsSecurity` at L373 and `isFileAndDirectorySecureSync` at L595.
  It is called from
  [`packages/cli/src/config/settings.ts` L995](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/packages/cli/src/config/settings.ts#L995).

### Correction to I2.md: every ancestor is checked

I2.md says the check covers the file "and its parent directory". At v0.67.0,
`isFileAndDirectorySecureSync` checks more than that:

1. Any ancestor directory that is a symlink must be owned by uid 0.
2. The parent directory **and every ancestor up to `/`** must be owned by uid 0
   and have neither `S_IWGRP` nor `S_IWOTH` set.
3. The file itself must pass the same check.
4. If the path resolves through a symlink, the resolved file and all of its
   ancestors must pass as well.

The sticky bit is ignored, so a system settings path under `/tmp` (mode 1777)
is always skipped. A missing file counts as secure and loads nothing
(`settings.ts` L991–993). As a result, the system settings path is
only enforced if the runner actually writes the file there. The row below uses
the corrected wording.

## Replacement 1 of 1 (section 8.3)

**Before** (present exactly once in the baseline):

```markdown
**Claude Code, Gemini CLI, OpenCode:** the same table must be filled in from each vendor's current headless documentation during WRK-11/12/13. That covers:
```

**After:**

````markdown
**Gemini CLI** (`@google/gemini-cli` 0.67.0, recorded by I2 from the vendor documentation at tag `v0.67.0`):

| Row | Gemini CLI 0.67.0 | Source |
| --- | --- | --- |
| Headless invocation | `gemini -p "" --output-format stream-json --approval-mode yolo --model <m> --skip-trust`, with the prompt on stdin. `-p` (or a non-TTY) selects headless mode. stream-json emits `init`, `message`, `tool_use`, `tool_result` and `result` JSONL events. `yolo` auto-approves tools, so nothing waits for a human. | [headless.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/cli/headless.md), [configuration.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/reference/configuration.md) |
| Config directory variable | `GEMINI_CLI_HOME` moves the whole user scope (`$GEMINI_CLI_HOME/.gemini`) to a per-attempt directory. `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names the system settings file, which outranks user and workspace settings. AQ writes `system-settings.json` there with `security.auth.enforcedType`, `telemetry.enabled=false`, `hooksConfig.enabled=false`, `tools.sandbox=false` and no auto-update. | [enterprise.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/cli/enterprise.md) (system settings, `GEMINI_CLI_HOME` per-job example), [configuration.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/reference/configuration.md) |
| **System settings must be root-owned** | The CLI ignores a system settings file unless the file, its parent directory and every ancestor up to `/` are owned by uid 0 and are not group- or other-writable. Symlinked paths are checked at their resolved target as well. The check is `isFileAndDirectorySecureSync` (with `checkPosixStatsSecurity`). An insecure file is skipped with "Security Warning: Skipping system settings file", and a missing file is treated as empty. Without `enforcedType` the CLI exits 41 with "Invalid auth method selected". The system settings file therefore sits in a root-owned, non-group/other-writable directory apart from the agent-writable home, never under `/tmp`. The runner writes it before launch and fails the attempt if it is absent. With a distinct agent uid (SBX7), the agent user must not own or be able to write that file or any directory above it. | [security.ts](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/packages/core/src/utils/security.ts), [settings.ts](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/packages/cli/src/config/settings.ts#L995); reproduced by I2 on the 0.67.0 bundle |
| Credential file and refresh | The Google login ("Sign in with Google", `oauth-personal`) is the google-auth-library `oauth_creds.json` in `$GEMINI_CLI_HOME/.gemini/`. The CLI refreshes it in place, which advances `expiry_date`. `GEMINI_FORCE_FILE_STORAGE=true` keeps it a file rather than an OS keychain entry (SBX19). Headless mode reuses a cached login. The lease (section 7) installs it as `.gemini/oauth_creds.json`, writes it back only when `expiry_date` advances, and wipes it together with the sibling files the CLI writes. | [authentication.mdx](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/get-started/authentication.mdx) (cached credentials; headless reuses them), bundle source (`GEMINI_FORCE_FILE_STORAGE`) |
| API-key routing through a loopback proxy | **Supported.** `GOOGLE_GEMINI_BASE_URL` points the client at the runner's loopback Gemini API proxy, and `GEMINI_API_KEY` holds only the proxy's attempt-local capability `aq-gemini-<64 hex>`. The proxy holds the real key in memory and swaps it into `x-goog-api-key`. It accepts only `/v1beta/models/<m>:(streamGenerateContent\|generateContent\|countTokens)`, and it refuses a wrong capability, any other path, or a response that echoes the key. The provider key never reaches the agent (SBX7). | [configuration.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/reference/configuration.md) (`GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`) |
| Native sandbox and kernel requirements | **No `native` mode in AQ.** Gemini's `--sandbox` runs tools in docker/podman, gVisor `runsc` or LXC on Linux (`sandbox-exec` on macOS), and nested container runtimes are not available in an AQ sandbox. Confinement is `container` only (`tools.sandbox=false`). A profile requiring `native` is refused. | [sandbox.md](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/cli/sandbox.md) |
| Subscription enrollment (vault capture) | **Supported as a file login** through the operator import path (section 7), as for Codex. Run `gemini` interactively once in an empty private home (`GEMINI_CLI_HOME=<dir> GEMINI_FORCE_FILE_STORAGE=true NO_BROWSER=true`), choose "Sign in with Google" and paste the code. Then import `<dir>/.gemini/oauth_creds.json` as the newest login version and delete `<dir>`. A one-off enrollment sandbox that captures straight into the vault is deferred, as is Codex device-auth under the 2026-11-04 C5b rescope. The Gemini login kind still needs aq-server support. | [authentication.mdx](https://github.com/google-gemini/gemini-cli/blob/v0.67.0/docs/get-started/authentication.mdx) |

Live qualification of these rows against Google is pending owner credentials (I2).

**Claude Code, OpenCode:** the same table must be filled in from each vendor's current headless documentation during WRK-11/13. That covers:
````

Nothing else in section 8.3 changes. The bullet list after "That covers:" and
the closing paragraph about limitations stay as they are.

## Wording changes from the I2 table

| I2 wording | 0005 wording | Reason |
| --- | --- | --- |
| "Finding: system settings ownership" … "file and its parent directory" | "System settings must be root-owned" … "the file, its parent directory and every ancestor up to `/`" | v0.67.0 source checks all ancestors (see the correction above). |
| "The container runner is uid 0, so this run complies. With a distinct agent uid (B6a2) …" | "With a distinct agent uid (SBX7) …"; adds "never under `/tmp`" and "fails the attempt if it is absent" | 0005 states the requirement, not one run's result. A missing file loads nothing, so it must be present. |
| "per-job directory" | "per-attempt directory" | 0005's term is attempt. |
| "The C6b `LoginKind::GeminiOauthCreds` leases it …" | "The lease (section 7) installs it …" | 0005 does not name aq-worker Rust types. |
| "`GeminiApiProxy`" | "loopback Gemini API proxy" | Same reason. |
| "cross-repo follow-up" | "still needs aq-server support" | Same reason. |
| "bundle source; follow-up filed" | security.ts / settings.ts links at `v0.67.0` | Gives a pinned public source. |
| "WRK-11/12/13" for the remaining harnesses | "WRK-11/13" | Gemini CLI (WRK-12) is now filled in. |

WRK-12 is "Qualify the Gemini CLI harness adapter" in aq-server
`docs/plans/0001-v2-milestones.md` (L56, at the baseline commit).

## Application and acceptance

1. Confirm that the target document's SHA-256 equals the baseline, or that the
   Before line occurs exactly once.
2. Replace the Before line with the After block.
3. Check: `grep -c '^\*\*Gemini CLI\*\* (`' <target>` returns 1;
   `grep -c 'isFileAndDirectorySecureSync' <target>` returns 1; and
   `grep -c 'Claude Code, Gemini CLI, OpenCode' <target>` returns 0.
4. Cite this handoff by aq-worker commit in the aq-server commit message.
