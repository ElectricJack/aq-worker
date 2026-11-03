# C3c — seed recipes and local publication preparation

Task `calm-delta-18.5`. **Acceptance is blocked: needs GHCR token; C3b also
refuses the inherited public GnuTLS library.** Neither template is admitted.
No GHCR image has been published, no GHCR visibility has been verified, and
no production allowlist has been changed. Scanner follow-up:
`calm-delta-18.5.1` (in this repository, integration `direct`). Supervisor
message `msg-6527cd77e4d643f3bb43b9ecdbc23bce` directed completion of local
work while operator provisioning remains pending.

Implementation: plan commit `eaebd78`; recipe/test source `cfbeedc` and later
evidence/harness refinements on `aq/calm-delta-18.5`. C3a bundle source remains
`c2b17674e7f463f5cde5bcf120a331859b65ef58`, independently recorded in
`C3c-evidence/installed-bundle.json`. No C3b code, schema, finalize check or
scanner policy is modified.

## Environment and source boundary

Linux x86_64/WSL2, Docker Engine 29.3.1, Python 3.12.3. Dedicated
`aq-c3c-qualification` docker-container builder uses
`moby/buildkit@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea`,
with two parallel build steps. The loopback-only registry binds
`127.0.0.1:5138`, image
`registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373`.
BuildKit's GHCR mirror points to that endpoint. No GHCR credential was supplied
or worked around. The intended namespace is lowercase `ghcr.io/electricjack`;
ownership/write access and public visibility remain unverified remotely.

`GHCR_TOKEN`, `GH_TOKEN` and `GITHUB_TOKEN` were absent; Docker's configured
credential helper had no stored `ghcr.io` credential. Only presence was
checked, never values. The description's `--project aq-platform` provisioning
message was refused with `project_id mismatch`. The authorized scoped project
is `aq-worker`; message `msg-89da77f615634886bd1f155b23c3962a` successfully
requested secure provisioning there. The supervisor was notified separately.
Verified wait registration is disabled by prime; no disabled wait was retried.

Recipes are public toolchain inputs only, with empty `prefetch` arrays. They
install checksum-verified upstream archives and packages from C3a's signed
Debian snapshot `20261030T000000Z`. `C3c-evidence/public-inputs.json` records
the exact URLs and hashes, including the independently checked pnpm SHA-512
metadata integrity. Rust is exactly 1.96.1 with rustfmt and clippy; Node is
24.11.0 and pnpm is 10.34.5. Tools work for UID/GID 10002 through
`/usr/local/bin`, without rustup or root. The private bundle Node 22.16.0
remains independent at `/opt/aq/node/bin/node`.

Qualification containers mount committed source archives only after image
assembly. They run as agent with two CPUs, 8 GiB memory, no capabilities and
no new privileges. No live checkout, host HOME or credentials are mounted;
containers are never committed. Rust builds have read-only source mounts and
separate writable Cargo/target directories. No private source or repository
objects are added to template layers. These are packaging/build proofs, not
hostile-code isolation or real-provider qualification.

## Observed images and admission

These digests identify manifests in the **local registry / OCI exports**.
GHCR names/tags below are intended destinations, not published references.

| Intended image and version tag | Observed manifest digest | Result |
| --- | --- | --- |
| `ghcr.io/electricjack/aq-sandbox-os:0.1.0` | `sha256:f6344f0456cbc11ef543b0060a808b4532c3a928b5b110d875065efbf9e066bd` | Preserved locally; no GHCR push |
| `ghcr.io/electricjack/aq-sandbox-runtime:0.1.0` | `sha256:13a64d6dfbedd8cb2e31b26a18670c71f6b43efd800eff94d62fc3b1d2bc2176` | Preserved locally; no GHCR push |
| `ghcr.io/electricjack/aq-template:rust-1` | `sha256:4c9c20007445c3a207f31787e635ec753e64b13f22300cd90030784f7862a1c5` | Built/finalized; credential scan refused |
| `ghcr.io/electricjack/aq-template:node-1` | `sha256:5e0e668ad48da42b5d46f03e971123bc9552c5721d14a147096a7250d2305710` | Built/finalized; credential scan refused |

C3a's local config IDs are different identities:
OS `sha256:876630cf2870b3af7f44620c0a9a40543c80a0c3511afa7edf9e76861ea7d063`;
runtime `sha256:e07bed781e32d30c17589d081b1f11b6aa896b60271731aa63d5a25daa32cca5`.
The base/runtime manifests use Docker schema 2; the template exports use OCI
manifests. Complete template manifest JSON, source config IDs and layer IDs
are in `C3c-evidence/*.manifest.json` and `c3a-images.json`.

Both C3b CLI builds completed BuildKit assembly and the fixed finalize RUN.
`rust-buildkit.log` and `node-buildkit.log` record successful image export.
Both then returned exit 2 from credential scanning. Independent `template
scan` commands also returned 2; exact sanitized output is in `rust-scan.json`
and `node-scan.json`. No provenance, bundle-manifest output, or allowlist file
was emitted by the build command after this refusal. A candidate entry was
not hand-authored. `build-observations.json` is explicitly diagnostic metadata,
**not** C3b provenance or an admission record.

Diagnostic scanning of the public inherited library
`/usr/lib/x86_64-linux-gnu/libgnutls.so.30.34.3` reproduces the first refusal.
Its ELF bytes hash to
`779b25d20249988bea2c1aa6bbeb218f5ae7ea8a9d30ce4f54ea37372965cc4b`.
The matcher finds nine PEM format delimiters and zero credential-label
assignments; all token-pattern matches are format delimiters. No matched
content was printed. See `public-library-scan.json` and
`rust-scan-diagnostic.json`. This failure precedes the toolchain layers; a
future scanner fix still needs to scan all remaining inherited/toolchain
content. No exemption, skipped layer or scanner weakening was introduced.

Large artifacts are retained outside Git under
`/tmp/aq-c3c-calm-delta-18.5`: `rust/image.oci.tar` (652846592 bytes,
SHA-256 `8f4b4b31079f9428ed2983bde2c8cebdf5b0b8737d4891b7ab1109f782854ecf`),
`node/image.oci.tar` (501209600 bytes,
SHA-256 `7ed46cfe482a7d887c25d40deb593a1b2250648680e00133c7983f3a2890448d`),
and base/runtime `os.transport` / `runtime.transport` Skopeo directories.
These are local temporary artifacts, not durable release storage; they must
be retained or transferred before host cleanup. Rejected OCI images remain
private forensic data and must not be publicly uploaded. Local imports use
the explicit `aq-template-unadmitted` repository and preserve the scanned
manifest bytes; they do not create allowlist records.

## Exact local commands and checks

All costly validation uses foreground `aq test` with its resource controls.
`aq task heartbeat calm-delta-18.5` was run before long work and renewed
during builds. Cargo wrapper: `docs/implementation/C1-evidence/test_cargo_check.py`;
it sets `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`.

```sh
docker run -d --name aq-c3c-registry --publish 127.0.0.1:5138:5000 \
  --mount type=volume,src=aq-c3c-registry,dst=/var/lib/registry registry:2
docker tag aq-sandbox-os:c3a 127.0.0.1:5138/electricjack/aq-sandbox-os:0.1.0
docker tag aq-sandbox-runtime:c3a 127.0.0.1:5138/electricjack/aq-sandbox-runtime:0.1.0
docker push 127.0.0.1:5138/electricjack/aq-sandbox-os:0.1.0
docker push 127.0.0.1:5138/electricjack/aq-sandbox-runtime:0.1.0
docker buildx create --name aq-c3c-qualification --driver docker-container \
  --driver-opt network=host \
  --driver-opt image=moby/buildkit@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea \
  --buildkitd-config /tmp/aq-c3c-calm-delta-18.5/buildkitd.toml --bootstrap default
```

The exact non-secret mirror config and build config are committed as
`C3c-evidence/buildkitd.toml` and `builder.staging.json`. The latter names
`allowlist.candidate.json` under `/tmp`, separate from any production vault.
Its `registry_visibility: public` describes the intended disclosure policy
(prefetch forbidden), not an observed GHCR ACL.

The worker was built through the C1 wrapper using
`C1_CARGO_COMMAND='["cargo","build","--locked","-p","aq-worker","--bin","aq-worker"]'`.
Each of these build commands was also run through that wrapper with
`C1_CARGO_CWD="$PWD"`, `PYTHONDONTWRITEBYTECODE=1`, and `aq test
docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s`:

```sh
target/debug/aq-worker template build packaging/sandbox/recipes/rust.json \
  --build-config /tmp/aq-c3c-calm-delta-18.5/builder.staging.json \
  --output /tmp/aq-c3c-calm-delta-18.5/rust
target/debug/aq-worker template build packaging/sandbox/recipes/node.json \
  --build-config /tmp/aq-c3c-calm-delta-18.5/builder.staging.json \
  --output /tmp/aq-c3c-calm-delta-18.5/node
target/debug/aq-worker template scan /tmp/aq-c3c-calm-delta-18.5/rust/image.oci.tar
target/debug/aq-worker template scan /tmp/aq-c3c-calm-delta-18.5/node/image.oci.tar
```

The standalone scan commands are diagnostics, both exit 2. C3b CLI build
wrapper tests both fail on that exit, as recorded in `rust-build.log` and
`node-build.log`; they are not counted as passing tests.

```sh
AQ_TEMPLATE_BINARY="$PWD/target/debug/aq-worker" PYTHONDONTWRITEBYTECODE=1 \
  aq test scripts/tests/test_seed_recipes.py scripts/tests/test_template_tool.py \
  scripts/tests/test_package_sandbox.py -n 0
cargo fmt --check
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","clippy","--locked","--all-targets","--","-D","warnings"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","test","--locked","-p","aq-worker","--test","templates","--test","runtime_bundle","--bin","aq-worker"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
C3C_QUALIFICATION=/tmp/aq-c3c-calm-delta-18.5/qualification.json \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3c-evidence/test_qualification.py \
  -k toolchain -n 0 -s
C3C_QUALIFICATION=/tmp/aq-c3c-calm-delta-18.5/qualification.json \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3c-evidence/test_qualification.py \
  -k 'committed_repository and not dashboard' -n 0 -s --timeout=3600
C3C_QUALIFICATION=/tmp/aq-c3c-calm-delta-18.5/qualification.json \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3c-evidence/test_qualification.py \
  -k dashboard -n 0 -s --timeout=3600
```

Recipe TDD: `recipes-red.log` reports two missing-recipe failures before the
recipes existed. Python area: **22 passed**, `python-final.log`. Worker CLI
build, fmt and clippy: passed. Rust area: **6 passed** (2 CLI unit, 1 runtime
bundle, 3 template integration), `rust-area.log`. Physical toolchain/finalize
smoke: **2 passed**, `toolchain-final.log`, including the independent bundle
Node version. Earlier physical logs retain setup failures; one Node pull
failed through the desktop credential helper. It was preloaded anonymously
from the loopback registry with an empty public Docker config; tests then
use `--pull=never`. No GHCR auth refusal was bypassed.

After synchronization, the same Python area again reported **22 passed**
(`python-post-sync.log`) and `cargo fmt --check` passed (`fmt-post-sync.log`).
`seed-invariants.json` records successful schema validation, canonical recipe
hashes, agreement of recorded Dockerfiles with C3b rendering, and OCI manifest
hashes. It also confirms that no candidate allowlist exists; it is diagnostic
metadata, not a separate test suite or admission evidence.

Repository proof source revisions and exact Docker commands are recorded in
`C3c-evidence/qualification.json` and the proof logs. Each source tree comes
from `git archive REVISION`, without sibling trees. Rust proofs execute
`cargo build --locked`; dashboard executes `pnpm install --frozen-lockfile &&
pnpm build`. All six repository proofs passed:

| Repository | Committed source revision | Result |
| --- | --- | --- |
| aq-server | `841e19ff6260ce7a97c600227e94c3c42c1abec5` | Passed; Cargo 8m 09s |
| aq-worker | `eaebd788085a7f74cfec49e6eb3b8763800c2ec3` | Passed; Cargo 5m 05s |
| aq-git | `e05d4e67c9bc53ed0b69ec3c8cff9fa9ad24c287` | Passed; Cargo 54.26s |
| aq-graph | `9e957b25a6e027aa30895535c4bc19859fa52883` | Passed; Cargo 2m 38s |
| aq-playbooks | `6798552f46360b7d1891b1ff9514b4ce71bfb6fc` | Passed; Cargo 1m 20s |
| aq-dashboard | `eee9d8396f1bb157d79ff31e1cdb583645e4f62d` | Passed; frozen install and production build |

`repository-proofs.log` reports **5 passed, 3 deselected in 1118.67s**;
`dashboard-proof.log` reports **1 passed, 7 deselected in 18.57s**. Cargo
durations in the table exclude archive/download setup; the full pytest
duration includes it. The worker proof intentionally uses the committed
pre-recipe source revision recorded before qualification; Rust source is
unchanged by C3c. The separate worker area suite tested the implementation
checkout. These six proofs do not establish credential-scan admission.

## Remaining publication/admission steps

1. **needs GHCR token**: only Jack can securely provision it. Never place it
   in recipes, evidence, logs, task messages or build arguments.
2. Resolve `calm-delta-18.5.1`; rebuild the operator CLI and rerun both C3b
   builds into fresh output directories. Require actual scan success and
   C3b-emitted provenance/allowlist entries. Update image/recipe pins to the
   newly observed digests; the diagnostic digests above are not admissions.
3. Publish base/runtime and successfully admitted template archives using
   Skopeo 1.21.0 with `copy --preserve-digests`. Qualification used the pinned
   official image
   `quay.io/skopeo/stable@sha256:a585e4a3b8a045baa87c7f1b2f940d6d299ebede85ab3f2419d52d2264eefc93`.
   Mount an operator-provided auth file only into this publisher, never a
   build context. Source examples are `dir:/artifacts/os.transport`,
   `dir:/artifacts/runtime.transport`, and `oci-archive:/artifacts/rust-admitted/image.oci.tar`.
   Destinations are the four version tags in the table. The prepared commands
   are not executed while the scan/token gates are unmet.
4. Read remote raw manifests and compare SHA-256 with the successfully scanned
   digests. Verify each GHCR package anonymously from a fresh publisher
   container without auth mounts; record the actual visibility result.
5. Install only the verified C3b-generated entries into the operator's
   production allowlist, preserving existing entries/materializations and its
   `<allowlist>.lock` plus atomic-replace discipline. No arbitrary digest-add
   shortcut or manually fabricated provenance is valid.

Prepared publication commands below have **not** been run. They require fresh,
successfully admitted `rust-admitted` and `node-admitted` outputs and an
operator-provisioned Skopeo-compatible registry auth file (a Docker Desktop
helper-only configuration is insufficient). The auth file is mounted only into
the separate publisher container. Run from the repository root in Bash:

```sh
set -euo pipefail
: "${C3C_REGISTRY_AUTH_FILE:?secure operator auth file required}"
test -r "$C3C_REGISTRY_AUTH_FILE"
test -s /tmp/aq-c3c-calm-delta-18.5/rust-admitted/provenance.json
test -s /tmp/aq-c3c-calm-delta-18.5/node-admitted/provenance.json
target/debug/aq-worker template scan /tmp/aq-c3c-calm-delta-18.5/rust-admitted/image.oci.tar
target/debug/aq-worker template scan /tmp/aq-c3c-calm-delta-18.5/node-admitted/image.oci.tar

c3c_publish() {
  docker run --rm --pull=never --cap-drop ALL \
    --security-opt no-new-privileges \
    --mount type=bind,src=/tmp/aq-c3c-calm-delta-18.5,dst=/artifacts,readonly \
    --mount "type=bind,src=$C3C_REGISTRY_AUTH_FILE,dst=/auth.json,readonly" \
    quay.io/skopeo/stable@sha256:a585e4a3b8a045baa87c7f1b2f940d6d299ebede85ab3f2419d52d2264eefc93 \
    copy --preserve-digests --authfile /auth.json "$1" "$2"
}
c3c_publish dir:/artifacts/os.transport docker://ghcr.io/electricjack/aq-sandbox-os:0.1.0
c3c_publish dir:/artifacts/runtime.transport docker://ghcr.io/electricjack/aq-sandbox-runtime:0.1.0
c3c_publish oci-archive:/artifacts/rust-admitted/image.oci.tar docker://ghcr.io/electricjack/aq-template:rust-1
c3c_publish oci-archive:/artifacts/node-admitted/image.oci.tar docker://ghcr.io/electricjack/aq-template:node-1

# Fresh containers, no auth mounts: anonymous visibility plus manifest hashes.
for c3c_image in aq-sandbox-os:0.1.0 aq-sandbox-runtime:0.1.0 aq-template:rust-1 aq-template:node-1; do
  docker run --rm --pull=never \
    quay.io/skopeo/stable@sha256:a585e4a3b8a045baa87c7f1b2f940d6d299ebede85ab3f2419d52d2264eefc93 \
    inspect --raw "docker://ghcr.io/electricjack/$c3c_image" | sha256sum
done
```

Compare the first two hashes with the preserved base/runtime manifest digests
and the last two with the **new successful** C3b provenance records before
applying any production allowlist change. A failed anonymous read leaves
visibility unverified and production admission unapplied.

No scheduled rebuild automation, server catalog mapping, backend snapshots,
architecture/spec changes or runtime behavior changes were made. Full-suite
CI remains integration-owned under prime's focused validation scope.

## Review and integration boundary

Author review checked the existing C3b parser against both recipes, download
checksums, agent PATH and permissions, private runtime/toolchain separation,
empty prefetch, finalize order, source mount boundaries, and exact manifest
preservation. C3c has no required adversarial review marker. Scanner policy and
runtime code remain unchanged, and the open scanner child remains actionable.

Prime requested merging `origin/dev`; that ref is absent, so the attempted
merge refused (`C3c-evidence/dev-merge.log`). `origin/HEAD` points to
`origin/main`. Guarded `aq git pull --branch main --brief` succeeded without
changing the tree; the main baseline remained
`9a00f05142957accfdb66cc7a7f40fe9e563356e`.

The prepared publication block was extracted and passed `bash -n`; no commands
in that block were executed. GHCR credential provisioning, remote publication,
anonymous visibility verification and production allowlist installation remain
unmet. The separate C3b scanner refusal must also be resolved before template
publication. Local packaging proofs cannot satisfy these acceptance criteria.

After the final proofs, the dedicated builder and loopback registry were
stopped with `docker buildx stop aq-c3c-qualification` and
`docker stop aq-c3c-registry`. Their configuration, cache/registry volume,
local image references and `/tmp/aq-c3c-calm-delta-18.5` artifacts are retained
for the operator and scanner follow-up; no unrelated Docker resources were
changed. To resume local work before host cleanup, start the registry with
`docker start aq-c3c-registry` and bootstrap the existing builder with
`docker buildx inspect aq-c3c-qualification --bootstrap`.

The branch was preserved on `aq/calm-delta-18.5` through guarded `aq git push`.
AQ task comments record the exact pushed evidence commits. The requested close
is `--outcome fail --work-outcome blocked`, because **needs GHCR token** and
the separate scanner refusal leave publication/admission acceptance unmet.
The first close attempt was refused because `calm-delta-18.5.1` is an open
child. Worker inspection of that child also refused `out_of_scope: task_id
mismatch`. The supervisor was asked to preserve that finding by moving it to
an appropriate container/root, allowing this task to close without abandoning
the unresolved scanner work. The task history records whether that close has
subsequently been accepted.
