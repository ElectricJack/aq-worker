# Spec 0007 phase 1, W13: terminal input for docker-local Claude

Spec: aq-server `docs/specs/0007-swarm.md` §5.4. Plan: aq-server `docs/superpowers/plans/2026-11-07-swarm-p1-workers-as-services.md`, task W13 (with S15). Branch `swarm/p1-terminal`, based on aq-worker main `b95b108` (#49 merged).

**Status: Step 1 passed. Implementation is stopped before any code.** The design needs a change in `crates/aq-session-host`, which the plan says not to modify without reporting first ("Blocker" below).

## Step 1: the probe

### Procedure

The probe ran in the pinned sandbox template `127.0.0.1:5138/electricjack/aq-template-unadmitted@sha256:057ae952…3950` (`~/.local/share/aq-ms1/worker/node.json` `docker_local.templates[0]`). It used the dedicated rootless Podman daemon at `/run/user/1000/aq-sandbox/podman.sock`. `claude --version` printed `2.1.318 (Claude Code)`.

The invocation is `remeasure.sh`'s, unchanged:
- `docker run --rm -i --pids-limit 512 -v <dir>:/p <template> /bin/bash /p/run.sh`, with the owner's subscription token file on stdin;
- the same environment, `settings.json` and flags;
- the same token handling: read from stdin into a shell variable, then set as `CLAUDE_CODE_OAUTH_TOKEN` on the claude process only.

There are two differences:
- `--input-format stream-json` instead of `text`;
- a feeder process writes the user messages to Claude's stdin as `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]}}` lines.

The scripts, messages, transcripts and timelines are in `0007-P1-terminal-evidence/probe{1,2}/`. They were checked for the token's exact bytes and for `aqn_|aqa_|aqe_|sk-ant`, and neither was found.

### Probe 1: two turns, then end of input

1. Message 1 is the `minimal` measurement prompt: "Reply with exactly OK and use no tools."
2. The feeder waits for the first `result` event and keeps stdin open and idle for 3 s.
3. Message 2: "Reply with exactly the text of your previous reply followed by the digit 2."
4. The feeder waits for the second `result` event, waits 3 s, then closes stdin.

| Event (unix ms) | |
| --- | --- |
| message 1 written | 1794130721759 |
| result 1 seen | 1794130726343 |
| message 2 written | 1794130729346 |
| result 2 seen | 1794130731359 |
| stdin closed | 1794130734364 |
| claude exited, rc 0 | 1794130734402 |

- Both turns produced a `result` event with `subtype: success` and `is_error: false`. The results were `"OK"` and `"OK2"`, so the second turn saw the first turn's context.
- There was one `session_id`. Each turn starts with its own `system init` event.
- Claude stayed alive with stdin open and idle between the turns.
- Closing stdin (EOF) ended the process 38 ms later with exit 0. Nothing was written to stderr.

### Probe 2: a line typed mid-turn, and EOF right after the first result

1. Message 1: "Run `sleep 8` with the Bash tool, then reply with exactly DONE1."
2. Message 2 is written 3 s later, while the tool runs: "Reply with exactly DONE2 and use no tools."
3. The feeder closes stdin as soon as the first `result` event appears. This is the plan's end-of-input rule.

What happened:
- The mid-turn message was folded into the running turn. There was one `result` event, with `success`, `num_turns: 2` and result `"DONE2"`. Claude did not start a second turn.
- EOF right after that result ended the process 37 ms later with exit 0.

### Conclusion

Claude Code 2.1.318 accepts typed lines as streaming user messages in print mode (`-p --output-format stream-json --input-format stream-json`). This needs no container change: no TTY, no attach and no `OpenStdin`. The default design stands, and owner question 9 (full TUI) does not arise.

The probe settles several design details:
- **End of input:** close stdin after the first `result` event when no message is waiting, as the plan says. Do not wait for one `result` per forwarded message: lines typed mid-turn share one `result`, so a per-message count would never close.
- **Transcript:**
  - Several `result` and `system init` events can appear. `aq_handoff_note::{proven_success, conclusion}` already read the last `result` event.
  - An owner line can change the agent's final message. If that message has no `Changes:` line, the attempt reports Failed. This follows from Q10, which accepts owner lines as part of the conversation.
- **Owner lines and the transcript:** without `--replay-user-messages`, owner lines do not appear in `claude.jsonl`. The model's replies to them do.

## Blocker: the session host refuses terminal input for the driver's startup profile

The driver (`aq-sandbox-driver`) is the pane program. `native/launch/claude_docker.rs:448` starts it with `ObservedStartup::native_exec`. Today `aq-session-host` allows terminal input only for the named read-only interactive profile:

- `observed/mod.rs:151`: `native_exec` refuses `input_enabled` with "native stdin profile does not support terminal input". `Spec::validate` refuses it again for `NATIVE_EXEC_PROFILE` (`observed/mod.rs:268-271`).
- `observed/mod.rs:252`: `Spec::interactive()` is true only for `named-readonly-interactive-pty-v1`. That flag decides the rest:
  - stdin stays the owned PTY. For `native_exec`, FD0 is the sealed startup prompt (`exec.rs:161`).
  - the PTY viewer is allowed ("PTY viewer requires explicit interactive profile", `terminal_io.rs:466`, `reader_lease.rs:259`).
  - the interactive permit bound applies (`lib.rs:393`, 3,600,000 ms instead of 120,000 ms).
- The interactive profile needs a `NamedReadonlyExecImage`. That is a separately qualified image in a read-only mount namespace (`observed/named_image.rs`), and it appends the verified prompt as the last argv after `--`. The docker-local node loads the driver from a memfd `NativeExecImage` pool, and its environment premise says "node programs pinned by SHA-256 on the WSL2 host (not a read-only image)".

So a manifest with `input_enabled = true` cannot start the Claude driver host. With input disabled, the server refuses a control lease (`terminal/service.rs:729`). The server checks only the sealed `input_enabled`. It never checks the startup profile name.

### Options

- **A. A fourth startup profile in `aq-session-host` (the smallest change).**
  - Add `native-exec-interactive-pty-v1`: the same memfd `NativeExecStdinV1` image the driver uses today, but requiring `input_enabled`.
  - `Spec::native_exec()` and `Spec::interactive()` include it.
  - It reuses the existing interactive branch: stdin stays the PTY, the command's last argument is `--`, and the host appends the verified prompt bytes as argv.
  - The lease, sealed-policy, no-replay and guardian checks in `terminal_io.rs` don't depend on the profile, so they are unchanged.
  - Scope: one constant, one constructor and one `validate` arm in `observed/mod.rs`, plus the two predicates and tests. It has its own lib count, not the aq-worker ratchet.
  - Review point: the interactive profiles would include a memfd-sealed native image as well as a named read-only image.
- **B. Run the driver as a named read-only image.** No session-host change, but a fixed node setup change. The driver needs a qualified read-only mount environment and named-image pins, and the ms1 environment definition changes, including its "not a read-only image" premise.
- **C. Keep docker-local Claude watch-only** until A or B is chosen.

## Other findings for the implementation

These hold under A or B:

1. **No control relay runs for Claude.**
   - `runner/claude_docker.rs:174` passes `terminal_clock: None` to `drive_running_owned`, so `start_phase_control` never runs.
   - The interactive profile needs an explicit trusted clock bound. Following the Codex precedent, that is an optional `terminal_clock_skew_ms` (≤ 1000) on the Claude executor config, required exactly when the definition is interactive. ms1's `native.json` (L0) must then set it.
2. **PTY echo.** The tmux pane's PTY echoes typed bytes before any guard runs (canonical mode, ECHO on), so a typed token would reach the pane and the output relay. The driver must turn ECHO off on its stdin and echo each accepted line itself through the `print_logs` secret check.
3. **The Codex viewport classifier runs on every input** (`control_relay.rs:104`). On the Claude driver pane it fails closed: input is refused if the pane shows "codex login" or "Sign in with ChatGPT", and is limited to Enter or Ctrl-C if it shows a trust-directory prompt. This is acceptable, and the file is left as it is.
4. **Recipe digest.** The server pins the default Claude recipe digest `b30030fb9192268da6079f59d650bcbad5dae3b2e34bf28af2b9e96e9fad8765`. It covers `sha256(sandbox_driver::RUNNER)`, so the default runner must stay byte-identical. The interactive runner is a separate constant, hashed into its own recipe body.
5. **End-of-input authority.** The runner script's stdout is Claude's stdin, so the feeder must write nothing else. The driver already sees the `result` events in the container log. The driver should decide end of input and upload an end marker after the last message. The runner then closes Claude's stdin on that marker, so the two can't disagree.
6. **Line ratchet.** The aq-worker lib is at its ratchet (70,917), and the PR's net change must be ≤ 0. The pure input gate (framing, the token guard, the pending bound, closing, the pane echo) and its tests go in `aq-handoff-note`. The driver loop, recipe, profile and clock glue in the aq-worker lib must be offset by moving lines out, for example `sandbox_driver.rs`'s inline tests into an existing integration test binary.
