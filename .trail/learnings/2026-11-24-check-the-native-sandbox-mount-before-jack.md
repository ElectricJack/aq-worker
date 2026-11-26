---
title: Check the native sandbox mount before offering work
author: Jack
date: 2026-11-24
branch: fix/held-worker-preflight
cost: { minutes: 24, tokens: 553550 }
id: l-2bcc91de
scope: repo
status: active
src: agent
touches:
  - crates/aq-worker/src/native/runner/admission/polling.rs
  - crates/aq-worker/src/native/runner/prepare.rs
---

## Decided

The live native stage worker uses InputPipeline and its own journal. Its disk reservation is a logical Git budget, so it never called the legacy driver's filesystem guard. Check the real journal mount and the sandbox daemon before polling, then recheck before accepting and before preparing a grant after hydration. An already reserved offer with no grant can settle through the existing server expiry. Publish a temporary readiness wait separately from the owner recovery hold so the next healthy poll can clear it without intervention.

## Tried and ruled out

The old controller's disk inventory measures graphroot, but native placement does not consume it. Podman's Docker-compatible /info supplies graphroot and omits a custom volume path. On the deployed Podman 5.8.8, its versioned libpod info supplies both paths. Use the less available of the measured mounts so a full relocated work-volume directory cannot be hidden behind a healthy graphroot.

## Watch out

A confirmed image-missing response permits a pull. On a create timeout, the daemon may have created the writable layer already, so the worker keeps the uncertainty and reports the operation.

In the code: Admission::poll_once, Preparation::collect, DockerLocalConfig::check_admission, and check_placement.
