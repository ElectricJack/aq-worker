# C3b Template Build Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Assemble pinned sandbox templates and admit only finalized, credential-scanned OCI digests with provenance.

**Architecture:** Embed Python standard-library packaging tools in the Rust CLI, preserving the existing packaging approach without runtime source-tree dependencies. BuildKit exports a single-platform OCI archive on an explicitly configured builder; a reusable scanner verifies descriptors and every layer before a locked, atomic allowlist update.

**Tech Stack:** Rust/clap, Python 3.12 standard library, Docker BuildKit, OCI image layout, JSON configuration/recipes/allowlist.

**Spec:** Task calm-delta-18.4; C3b in aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml. C3a's packaging/sandbox/bundle_manifest.py defines the consumed manifest.

## Global Constraints

- Builds: each repository builds and tests alone; no sibling path dependencies, submodules or floating release inputs. Rust uses the committed lockfile with `--locked`.
- Authority: aq-graph owns task state; AQ owns grants, attempts and dispositions.
- Networking/hosting: no new listeners or attempt execution paths; template builds use a configured build node, outside attempt capacity.
- Secrets: no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions. Node/backend/Git/supervisor credentials never enter sandboxes.
- Isolation claims: do not claim hostile-code isolation for shared-kernel containers.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- Definition of done: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, named workspace tests and related area tests through `aq test`. Prime bounds tests to the changed area, with full-suite CI owned by integration.
- Evidence: docs/implementation/C3b.md records commands, environment, source commit and limits. Push through guarded AQ publication.
- No binary, service or CLI is installed as `aq` or `agent-queue`.
- C3b has no adversarial marker. Do not edit architecture or docs/specs files.

## Review Focus

- Credentials deleted by later whiteouts must still fail the scan.
- OCI descriptor substitution, traversal, duplicate entries and unsupported compression must refuse.
- Failed builds/finalize/scans must leave an existing allowlist unchanged.
- Recipe shell/Dockerfile injection, secret-shaped inputs and public prefetch must refuse before building.
- Provenance must name the OCI manifest digest, not Docker's local config ID, and advertise installed harness versions.

### Task 1: Recipe, finalize and OCI scanner

**Files:** Create `packaging/sandbox/template_tool.py`, `packaging/sandbox/credential_scan.py`, `scripts/tests/test_template_tool.py`, `crates/aq-worker/tests/templates.rs`.

**Interfaces:** Consumes schema-1 bundle manifest; produces `validate_recipe(recipe, config)`, `dockerfile(recipe, base, config)`, fixed `FINALIZE`, and reusable `scan_oci(path)` returning manifest digest and installed bundle manifest. Errors are sanitized `ValueError`s.

- [ ] Write Python tests with hand-built OCI fixtures and Rust acceptance bridges. `finalize_refuses_template_missing_tmux_or_users` runs the fixed finalize shell in a controlled PATH with missing tmux/users and expects nonzero; the complete fixture succeeds. `credential_scan_fails_build_with_auth_json` scans a real archive containing `.codex/auth.json` in an inherited layer, even when whiteouted later, and expects refusal.
- [ ] Run named tests through the C1 cargo wrapper; expect failure for absent tools.
- [ ] Implement bounded JSON recipe validation, pinned parent resolution, fixed finalize checks and descriptor-verified streaming OCI scanning, including sensitive paths, content and provider environment variables.
- [ ] Test malformed/oversized/secret inputs, custom CODEX_HOME, Claude/Gemini/cloud credentials, symlink targets, broken blob hashes, whiteouts, unsupported manifests and clean harness extraction.
- [ ] Run the Python area suite through `aq test`; expect all cases pass. Commit the implementation.

### Task 2: Build node orchestration, provenance, allowlist and CLI

**Files:** Modify `template_tool.py`, `crates/aq-worker/src/main.rs`; create `crates/aq-worker/src/template.rs`, `packaging/sandbox/templates.md`; modify `crates/aq-worker/src/lib.rs` and template tests.

**Interfaces:** Consumes Task 1 scan result; produces `build(recipe_path, config_path, output_path)` and JSON schema-1 allowlist entries with name/version, image reference/digest, provenance, harness versions and extensible materializations. Rust `template::run(args: &[OsString]) -> anyhow::Result<()>` embeds both Python files and forwards CLI status.

- [ ] Write build tests using a strict executable Docker boundary double that exports real OCI fixtures. Assert clean build returns the manifest digest, provenance pins and harnesses, updates the allowlist, and renders final bundle COPY followed by fixed finalize. Assert failed build/scan never updates it; stale conflicts and malformed existing entries refuse.
- [ ] Run tests; expect failure for missing build entry point/CLI.
- [ ] Implement an isolated context, explicit Docker context/BuildKit builder, pinned parents/runtime, optional private-only pinned Git prefetch, universal-base prerequisite installation, OCI export and scanner gating, locked atomic allowlist publication. Never forward build logs or credential-bearing environment to BuildKit. Keep OCI output for later publication by C3c.
- [ ] Add `aq-worker template build <recipe> --build-config <file> --output <new-directory>` and `template scan <oci-archive>`; require Python 3 and Docker BuildKit explicitly. Document JSON bounds, trust boundaries, builder setup, manifest digest and C7/C8 interfaces.
- [ ] Run area tests and CLI smoke, named workspace filters, fmt and clippy. Commit changes and evidence, squash local feature commits to one tree-preserving commit before review, sync default branch, push via AQ and close with evidence.

### Task 3: Evidence and review

- [ ] Record exact red/green commands, observed test counts, environment, source commit and physical-build limitations in `docs/implementation/C3b.md`.
- [ ] Review the whole branch against the five Review Focus cases; obtain the executing-plans fresh review and resolve material findings with regression tests.
- [ ] Mark AQ subtasks as done, publish the final SHA and close with `--changes`, `--verification` and both named `--test` commands.
