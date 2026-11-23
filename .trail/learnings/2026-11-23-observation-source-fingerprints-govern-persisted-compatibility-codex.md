---
title: Observation source fingerprints govern persisted compatibility
author: Codex
date: 2026-11-23
branch: codehealth/nov22
cost: { minutes: 672, tokens: 1602180 }
id: l-1a1a45c8
scope: repo
status: active
src: agent
touches:
  - crates/aq-observation-engine/src/normalize.rs
  - crates/aq-observation/src/normalize.rs
  - crates/aq-observation/src/spool.rs
  - crates/aq-observation-store/src/budget.rs
  - crates/aq-observation-store/src/lib.rs
---

## Decided

Keep the observation parser and store fingerprint inputs byte-for-byte stable during readability refactors. Refactor discovery, diagnostics, examples, tests and other code outside those inputs freely. Treat an intentional edit to fingerprinted code as a compatibility change with its own migration or verified replay plan.

## Tried and ruled out

A trial extraction in the Claude, Codex and OpenCode parsers and the row normalizer changed the parser artifact digests and the generated provenance. Restoring the original source bytes restored the digests. Updating the JSON goldens would verify the new output but would leave persisted source records and store identity markers from existing installations behind.

## Watch out

The hash covers Rust source bytes. Moving a helper or changing formatting can change which parser a running source is allowed to use. The reference spool and row engine check the persisted parser before collection. The row store also records an implementation identity derived from several Rust modules and checks it when opening an existing store. Exercise resume against previous-release state when changing either contract. Re-running the new binary against an empty store cannot cover that path.

## In the code

The parser input lists live in both observation crates' normalize.rs files. Their normalizers and the reference spool check the saved identity before applying input. The row store input list is in aq-observation-store/src/budget.rs. The reparse integration tests cover explicit replay of retained observations and the example export checks the golden provenance.
