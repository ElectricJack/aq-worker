# C3c Template Publication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish digest-pinned sandbox OS/runtime artifacts and qualify Rust 1.96.1 and Node 24 templates through the existing C3b gates.

**Architecture:** Consume C3a's existing images and C3b's unchanged schema-1 recipe/build/scanner interfaces. Keep toolchain installation independent of private source, qualify committed repository archives only in disposable containers after image assembly, and preserve the scanned OCI manifest digest during publication. While GHCR credentials are unavailable, use a loopback-only registry mirror and an isolated staging allowlist; neither constitutes production publication or production admission.

**Tech Stack:** Docker Engine 29.3.1, pinned BuildKit, OCI archives, C3b Rust CLI/Python tools, Rust 1.96.1, Node 24.11.0, pnpm 10.34.5, pytest through AQ.

**Spec:** Task calm-delta-18.5; C3c and Global Constraints in aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml and 2026-11-02-v2-full-application-plan.md. Consumed interface: packaging/sandbox/templates.md.

**Execution status:** Recipe implementation and local assembly/finalize are complete.
Both credential scans refuse an inherited public GnuTLS ELF; follow-up
`calm-delta-18.5.1` owns that C3b issue. No candidate allowlist/provenance was
emitted and no GHCR image was published. Exact external residual: **needs GHCR
token**. The supervisor directed preservation of local work and a truthful
blocked close if provisioning remains unavailable. Unchecked acceptance steps
below are not represented as successful.

## Global Constraints

- Each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked`.
- No wire contracts, lifecycle authority, attempt hosting or stop-proof behavior changes.
- No secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions. Node, backend, Git remote and supervisor credentials never enter sandboxes.
- Do not claim hostile-code isolation for shared-kernel containers.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- Definition of done: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and the focused Rust area suite through AQ resource controls. The task also requires five `cargo build --locked` repository proofs and the dashboard `pnpm install --frozen-lockfile` / `pnpm build` proof.
- Evidence goes in `docs/implementation/C3c.md`, recording exact commands, environment, commit and remaining limits. Changes are pushed through the project's integration policy. Prior qualification statements are not reused for changed code.
- While v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- C3c has no `review: adversarial` marker. Do not edit architecture or docs/specs files.
- A filter matching zero tests is not a passing named test. No private repository objects enter the images.
- Supervisor message msg-6527cd77e4d643f3bb43b9ecdbc23bce authorizes finishing local qualification while publication remains blocked. Exact residual: **needs GHCR token**. Never fabricate publication, registry visibility or production admission.

### Task 1: Pin public inputs and write recipes test-first

**Files:** Create `packaging/sandbox/recipes/rust.json`, `packaging/sandbox/recipes/node.json`, `packaging/sandbox/recipes/README.md`, `scripts/tests/test_seed_recipes.py`.

**Interfaces:** Consume `template_tool.validate_config`, `validate_recipe`, `dockerfile` and `credential_scan.secret_shaped` unchanged. Produce schema-1 recipes named `rust` and `node`, version `1`, with a GHCR digest-pinned OS parent, public checksum-verified downloads and empty `prefetch` arrays.

- [x] Resolve the existing OS/runtime manifest digests in a loopback-only registry, using `docker tag` and `docker push` only against that loopback endpoint. Record the original C3a config IDs and source provenance.
- [x] Download official Rust 1.96.1 and Node 24.11.0 checksums, and pnpm 10.34.5 package metadata/tarball. Verify the public archive bytes; record source URLs and fixed hashes in the recipes and evidence.
- [x] Write this failing recipe contract test before creating either recipe:

```python
@pytest.mark.parametrize("name", ["rust", "node"])
def test_seed_recipe_uses_c3b_contract_without_repository_objects(name):
    recipe = json.loads((RECIPES / (name + ".json")).read_text())
    assert recipe["name"] == name
    assert recipe["version"] == "1"
    assert recipe["prefetch"] == []
    config = build_config(recipe["parent"])
    tool.validate_config(config)
    tool.validate_recipe(recipe, config)
    rendered = tool.dockerfile(recipe, recipe["parent"], config)
    assert rendered.index("COPY --from=") > rendered.rindex("RUN [", 0, rendered.index("COPY --from="))
    assert not tool.scan.secret_shaped(rendered)
```

- [x] Run `PYTHONDONTWRITEBYTECODE=1 aq test scripts/tests/test_seed_recipes.py -n 0`; expect missing recipe failures, retaining the red log.
- [x] Install only selected standalone Rust components (rustc, cargo, target std, rustfmt, clippy) into `/opt/rust`, with executable links on the inherited project PATH. Install build-essential, pkg-config, libssl-dev, cmake, curl, xz-utils and Python 3 from the inherited signed Debian snapshot. Avoid rustup home dependence for agent UID 10002.
- [x] Install checksum-verified Node into `/opt/node` and pnpm into `/opt/pnpm`, linking project executables under `/usr/local/bin`. Keep the runtime's private `/opt/aq/node` unchanged. Install build prerequisites for native Node modules and remove archives within each installation layer.
- [x] Run the recipe contract and existing Python packaging area suites. Commit recipes, tests and documentation.

### Task 2: Physical assembly, scanning and repository proofs

**Files:** Create `docs/implementation/C3c-evidence/test_qualification.py`, `docs/implementation/C3c-evidence/` logs, build configurations, recipe checksums, provenance, scan results and a clearly named `allowlist.candidate.json`. Large OCI archives and disposable private source archives stay in `/tmp/aq-c3c-*`, outside Git.

**Interfaces:** Consume `aq-worker template build RECIPE --build-config CONFIG --output NEW_DIRECTORY` and `template scan OCI_ARCHIVE`; produce real OCI manifest digests and C3b-generated provenance, never hand-authored admitted entries.

- [x] Create a dedicated docker-container BuildKit builder with a pinned image, `network=host`, and a GHCR mirror pointing only to the loopback registry. Configure an isolated allowlist path, not a production vault.
- [ ] Build the current embedded `aq-worker` CLI through the existing C1 AQ resource-controlled wrapper if needed. Run both recipes through that CLI. Finalize and the inherited-layer credential scanner must pass unchanged; retain exact failures if either refuses.
- [ ] Rescan both complete OCI archives through `aq-worker template scan`. Copy the small canonical recipes, Dockerfiles, bundle manifests and provenance to evidence. Preserve large artifacts outside Git and record their hashes/paths.
- [x] Import each scan-refused OCI image into the explicitly unadmitted local registry for private diagnosis only with `skopeo copy --preserve-digests`, using a pinned official Skopeo container and no GHCR credentials. Verify the registry manifest digest equals the scanned digest before running it.
- [x] Add physical pytest checks: Rust tools report exactly 1.96.1 and rustfmt/clippy exist under agent UID 10002; Node reports 24.11.0 and pnpm reports 10.34.5; C3b finalize passes; bundle Node remains 22.16.0.
- [x] Export each selected repository's recorded committed tree using `git archive`. Mount one source tree at a time into a disposable qualification container, give it independent writable build/cache directories, and run `cargo build --locked` with two jobs for aq-server, aq-worker, aq-git, aq-graph and aq-playbooks. Do not commit containers or copy source back into template layers.
- [x] Run dashboard `pnpm install --frozen-lockfile` and `pnpm build` in its disposable container. All physical checks run through foreground `aq test`; heartbeat before long commands and renew during them. Any unavailable or failing proof remains explicitly unmet.

### Task 3: Prepare publication, verify and preserve work

**Files:** Create `docs/implementation/C3c.md`; update the recipe README with exact operator publication/admission commands and the staging/production distinction.

- [ ] Prepare digest-preserving GHCR publication commands for the OS/runtime version tags and both template version tags. Registry login uses operator-managed Docker settings; no credential appears in commands, logs or files. Verify public visibility anonymously after actual publication. Until provisioning arrives, leave every GHCR write and production allowlist update unapplied.
- [x] Document source SHAs, local config IDs versus OCI manifest digests, archive locations/hashes, exact build/proof commands, scanner output, provenance and the unresolved GHCR visibility check.
- [x] Run the Python packaging area and physical qualification through `aq test`. Run worker fmt, clippy and focused template/runtime CLI tests through the C1 wrapper, obeying the resource caps; full-suite CI remains integration-owned.
- [x] Self-review recipe/install/PATH behavior, image source boundary, finalize/scan evidence, digest preservation and all acceptance items. C3c has no required cross-family review.
- [x] Before close, merge `origin/dev` as prime requests; if absent, record the refusal and synchronize the actual default branch using guarded AQ pull. Run targeted checks after synchronization, commit and publish only the assigned task branch with `aq git push`.
- [ ] If the operator provisions GHCR, finish publication and visibility checks and install the verified allowlist through the authorized configuration workflow, then close pass. Otherwise preserve all reviewable local work and close fail with **needs GHCR token**, or park if explicitly directed. Never close pass on local-only evidence.


### Recorded deviations and residuals

- Both C3b builds completed assembly and finalize, then refused credential
  scanning (exit 2). Independent rescans also refused. The requested C3b
  provenance and candidate allowlist therefore do not exist. Diagnostic
  observations are explicitly labeled and are not substituted for admissions.
- Private qualification of these rejected artifacts used only the explicitly
  unadmitted loopback repository, preserving manifest digests. All five Rust
  repository builds and the dashboard build passed; physical toolchain/finalize
  checks passed, as did the focused worker definition-of-done checks.
- Publication commands are prepared and syntax checked; anonymous GHCR
  visibility and production allowlist installation remain unapplied.
- `origin/dev` is absent. Guarded AQ pull of `main` succeeded unchanged; the
  Python area (22 passed) and fmt were rechecked after synchronization.
- Temporary builder and loopback registry were stopped after qualification;
  archives, image references and volumes are retained outside Git for follow-up.
- Close fail/blocked with exact residual **needs GHCR token** and scanner
  follow-up `calm-delta-18.5.1`; preserve the pushed task branch. Do not close
  pass on these local-only proofs.
