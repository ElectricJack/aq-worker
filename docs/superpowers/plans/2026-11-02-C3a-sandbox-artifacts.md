# C3a Sandbox Artifacts Implementation Plan

> **For agentic workers:** Implement this plan inline, test-first, in the assigned AQ worktree. Steps use checkbox syntax for tracking. The task explicitly requires completion in this session.

**Goal:** Build pinned `aq-sandbox-os` and `/opt/aq` runtime artifacts with an installed-byte manifest and physical layer-change evidence.

**Architecture:** Separate OS inputs from runtime inputs and build contexts. The OS installs Git, tmux, bubblewrap, CA certificates, dynamic libraries and runner/agent users from a digest-pinned Debian base and a dated Debian snapshot. A scratch runtime image holds only `/opt/aq`; the runner builds from a committed archive with `cargo build --locked`. Its manifest hashes the installed binaries, including published aq-git v0.2.0, Codex 0.166.0 and private Node. Downstream templates copy the entire tree in their final layer.

**Tech Stack:** Rust integration tests, Python standard library context preparation/manifest generation, Docker BuildKit, Debian bookworm, Rust 1.96.1.

**Spec:** AQ task calm-delta-18.3; C3a in aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml, with its plan's Global Constraints.

## Global Constraints

- Each repository builds alone; use the committed Cargo.lock with `--locked`.
- No sibling source dependencies, submodules or floating release inputs.
- No secrets in images, manifests, logs or contexts. Export only explicit committed source paths and checksum-verified public artifacts.
- Keep wire contracts and lifecycle authority unchanged; no additional listeners.
- No v2 binary is installed as `aq` or `agent-queue`.
- Record exact commands, environment, commit and limits in docs/implementation/C3a.md.
- Run fmt, clippy and scoped tests through AQ resource controls; full CI belongs to integration per worker prime.
- C3b owns recipes/finalization/scanning/allowlists; C3c owns templates and publication; B6b owns privilege/proxy behavior; B7 owns aq-agent.

## Review Focus

1. Corrupt public inputs must fail checksum verification before image build.
2. Untracked/private worktree files must never enter either context.
3. Harness pin changes must leave OS context and layers unchanged.
4. Manifest digests must describe actual installed bytes, not merely copy input declarations.
5. Private Node and harness entrypoints must work independently of project PATH.

### Task 1: Pinned artifacts and their executable contract

**Files:** Create packaging/sandbox/{pins.json,Dockerfile.os,Dockerfile.runtime,bundle_manifest.py,README.md}, scripts/package-sandbox.py, crates/aq-worker/tests/runtime_bundle.rs, scripts/tests/test_package_sandbox.py.

**Interfaces:** `package(revision, inputs: Path, output: Path)` produces independent `os/` and `runtime/` contexts and build metadata from one pins file. `bundle_manifest.py ROOT` emits `ROOT/manifest.json` schema 1 with harness name/version/path/sha256, runner/aq-git versions/digests, Node version/digest, source revision and proxy placement. Runtime uses `/opt/aq/bin`, `/opt/aq/node`, `/opt/aq/harnesses/codex`.

- [ ] Write `runtime_bundle_lists_pinned_harness_versions`: prepare a miniature bundle with fixture bytes; run the actual manifest generator; compare Codex version with `aq_codex_mapper::CODEX_CLI_VERSION` and the pins; verify runner/aq-git/Node hashes against installed bytes. Add packager tests for corrupt inputs, private-file exclusion and OS-context stability across harness pin edits.
- [ ] Run tests through the existing C1 AQ Cargo wrapper; expect failure because packaging files are absent.
- [ ] Implement pins, definitions and context generator. Require immutable FROM arguments supplied by the generator. Verify all downloaded/extracted public inputs and worker archive. Use the published aq-git v0.2.0 binary, not the missing historical revision. Include aq-session-host as the runner's existing execution dependency; do not add aq-agent.
- [ ] Run the named acceptance command and the Python packaging area tests; expect nonzero matching test count and all pass.
- [ ] Commit the implementation.

### Task 2: Physical build and layer evidence

**Files:** Create docs/implementation/C3a.md and docs/implementation/C3a-evidence/ (small JSON records and test logs).

**Interfaces:** OS image and scratch bundle are local validation artifacts only; final publication belongs to C3c.

- [ ] Prepare public inputs; build both images from fresh generated contexts. Inspect OS tooling/users and runtime installed manifest, executable versions and loader closure.
- [ ] Build a temporary prior Codex 0.165.0 bundle and then the qualified 0.166.0 bundle using the same worker source, OS and build inputs. Compare real Docker RootFS layer digests in an ephemeral composition whose final instruction copies `/opt/aq`; record all unchanged base layers and the changed final bundle layer. This is a layer experiment, not qualification of the prior harness or production template assembly.
- [ ] Run fmt, clippy, named workspace test and packaging area checks with two Cargo jobs/test threads. Record unavailable checks truthfully.
- [ ] Review the final diff against all acceptance items; no cross-family review is required by the C3a contract.
- [ ] Squash the unpublished leaf implementation history into one commit, merge origin/dev as required, rerun targeted checks, publish with guarded aq git push, and close with evidence.
