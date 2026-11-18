---
title: "Sandboxed attempts run the interactive CLI; the turn's stop hook ends the attempt"
author: Jack
date: 2026-11-18
branch: feat/sandbox-interactive-tmux
cost: { minutes: 567, tokens: 1271762 }
id: l-028165d7
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/sandbox_driver.rs
  - crates/aq-handoff-note/src/harness.rs
  - crates/aq-worker/src/bin/aq-worker/manual_stream.rs
---

## Decided
Docker-local attempts run the interactive Claude/Codex TUI on the container's terminal. The node driver (crates/aq-worker/src/sandbox_driver.rs drive) attaches that terminal to its session-host pane and masks the attempt's login in the output before writing to the pane (crates/aq-sandbox-archive/src/pane.rs ExactRedactor, then redact::Redactor), so nothing after the pane ever holds it. The attempt ends when the turn ends: Claude's Stop/StopFailure hooks and Codex's notify write /aq/out/stop.json (aq_handoff_note::harness STOP_HOOK, stop_stream) and the driver's loop stops the sandbox and claims from last_assistant_message. Viewing and typing go through the terminal-relay companion (manual_stream.rs attempts/Attempt::viewer): an ordinary tmux client on the attempt's session-host socket, the same live stream as hand-started agents.

## Tried and ruled out
Enabling the finite terminal input relay for docker-local (manifest terminal.input_enabled, aq-admission PhasePolicy, a control relay clock): it needs an admission change in two repos, and a newly opened pane stays blank because the server keeps frames only while watched and the dashboard cannot draw the worker's renderer snapshot. The control relay also classifies the viewport with Codex-only text rules (control_relay.rs, publish_output.rs: 'codex login', 'sign in with chatgpt'), which would quarantine a Claude session that merely displays those words.

## Watch out
tokio::io::stdin would keep the process from exiting, so Relay reads the pane on a plain thread. Relay::finish must await the aborted output task, or the pane writer stays open (a scripted test hung on a pipe that never closed). The session-host local_tmux tests need --features test-support, without it io_setup times out with 'local process predicate timeout'. tmux's status line takes one row in those fixtures, so a 100x30 client gives a 100x29 window. The runner bytes are part of both recipe digests: any runner edit is a coordinated server, worker and node-template rollout.
