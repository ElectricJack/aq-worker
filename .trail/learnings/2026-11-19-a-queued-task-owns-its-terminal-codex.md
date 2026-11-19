---
title: A queued task owns its terminal size
author: Codex
date: 2026-11-19
branch: main
id: l-ae0d7b7e
scope: repo
status: active
src: agent
touches:
  - crates/aq-session-host/src/supervisor.rs
  - crates/aq-session-host/src/terminal_io.rs
---

## Decided

A queued attempt owns its terminal grid at launch. Its browser viewer uses the pane's rows and columns and pans locally. Repaint attaches another disposable tmux client to read a fresh screen. The finite keyboard lease remains the keyboard authority when it is offered.

## Tried and ruled out

The literal relay tail cannot reconstruct the current screen after a gap. Typing a redraw shortcut would be application input. tmux paste-buffer -r preserves ESC. An arbitrary byte cut before the relay can instead produce two independently delivered commands, so it must be made at a complete terminal input boundary.

## Watch out

There are two viewer resize paths for a queued attempt. Its finite control path targeted the actual pane with resize-window. The raw attempt stream used a browser-sized ordinary tmux client under window-size latest. Fixing one leaves the other available. The helper is pinned separately from the worker. Existing panes take their fixed geometry on a new launch with the new helper.

In the code:
- crates/aq-session-host/src/supervisor.rs pins the window; terminal_io.rs refuses finite Resize effects.
- crates/aq-worker/src/bin/aq-worker/manual_stream.rs attaches and repaints a viewer at the pane's actual geometry.
