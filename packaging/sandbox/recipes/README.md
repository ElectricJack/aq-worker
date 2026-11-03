# Seed toolchains

`rust.json` and `node.json` are schema-1 C3b recipes with no repository prefetch.
Their common parent is the C3a OS manifest digest. The Rust template supplies
Rust 1.96.1, rustfmt, clippy and C/C++ build prerequisites. The Node template
supplies Node 24.11.0, npm and pnpm 10.34.5, plus native-module prerequisites.
Project tools are linked under `/usr/local/bin`, usable by the agent account
without root or a rustup home. The runtime bundle keeps its independent Node
22.16.0 under `/opt/aq/node`; it is copied last by C3b.

Every downloaded toolchain archive is SHA-256 checked before extraction and
removed in the same installation layer. Rust's checksum comes from
`https://static.rust-lang.org/dist/rust-1.96.1-x86_64-unknown-linux-gnu.tar.xz.sha256`;
Node's comes from `https://nodejs.org/dist/v24.11.0/SHASUMS256.txt`. pnpm's archive
was additionally verified against the SHA-512 integrity in
`https://registry.npmjs.org/pnpm/10.34.5`. Debian packages use the signed snapshot
inherited from C3a (`20261030T000000Z`). These pins describe inputs; observed
OCI digests describe outputs.

Build with the existing CLI and an explicitly provisioned build-node config:

```sh
target/debug/aq-worker template build packaging/sandbox/recipes/rust.json \
  --build-config /tmp/aq-c3c-calm-delta-18.5/builder.staging.json \
  --output /tmp/aq-c3c-calm-delta-18.5/rust
target/debug/aq-worker template build packaging/sandbox/recipes/node.json \
  --build-config /tmp/aq-c3c-calm-delta-18.5/builder.staging.json \
  --output /tmp/aq-c3c-calm-delta-18.5/node
```

See `../templates.md` for config and admission semantics. Existing output
directories and duplicate name/version admissions refuse; use a fresh staging
directory for a fresh build. Do not replace the production allowlist with a
staging file: it may have existing entries and backend materializations.

## Publication gate

C3c requires a securely provisioned GHCR credential before these images can be
published. No token is a recipe input. The local qualification uses a
loopback-only registry mirror with the intended GHCR repository paths and exact
base/runtime manifest bytes. A staged `allowlist.candidate.json` records
successful local C3b admission only; it is not the active production allowlist,
and its GHCR references are not pullable until publication succeeds.
The current local builds fail credential scanning on an inherited public
GnuTLS library, so no candidate allowlist or C3b provenance was emitted.
Follow-up `calm-delta-18.5.1` tracks that refusal. Do not publish these rejected
archives or fabricate admission records.

The exact built digests, artifacts, commands and unresolved checks are recorded
in `docs/implementation/C3c.md`. A local build cannot establish GHCR visibility.
After secure operator login, preserve the scanned OCI manifest with Skopeo's
`copy --preserve-digests` and compare the remote manifest digest before any
production admission. Do not use `docker load` followed by `docker push` for
templates: conversion can change the scanned manifest's media types or bytes.
