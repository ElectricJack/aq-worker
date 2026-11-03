# C3a — sandbox OS and runtime artifacts

Implementation source: `c2b17674e7f463f5cde5bcf120a331859b65ef58`, based on
`aa6c807`. The subsequent evidence-only commit preserves that source commit
in branch history. Task: `calm-delta-18.3`. Environment: Linux x86_64,
Docker Engine 29.3.1/BuildKit, rustc 1.96.1, Python 3.12.3. No registry
publication or live credential/provider qualification is performed by C3a.

## Delivered interface

- `packaging/sandbox/Dockerfile.os`: pinned Debian bookworm, Git 2.56.0,
  tmux, bubblewrap, certificates, runtime libraries, Perl for Git helpers,
  runner UID/GID 10001 and agent UID/GID 10002. Root starts the runner;
  privilege drop and proxy behavior remain B6b's responsibility.
- `packaging/sandbox/Dockerfile.runtime`: scratch transport image containing
  `/opt/aq`; locked runner/session-host build, published aq-git 0.2.0, Codex
  0.166.0, private Node 22.16.0 and an installed-byte manifest. The existing
  session host is required by the runner's guardian/tmux execution path.
  aq-agent and separate proxy binaries are excluded as required by the scope.
- `packaging/sandbox/pins.json`: the single input pin file. Builder/base use
  immutable image digests. Debian's signed snapshot `20261030T000000Z` fixes
  package versions and verifies package checksums through apt. Git, aq-git,
  Codex's extracted native executable and Node's tarball have SHA-256 pins.
- `scripts/package-sandbox.py`: independent OS/runtime contexts from explicit
  committed source paths and checksum-verified public files. No private homes,
  arbitrary inputs, dirty files, `.aq` or `.git` are exported. Both Docker
  builds recheck their input checksums. Runtime-only pins never enter the OS
  context. Definitions are rendered from the selected source revision.
- `packaging/sandbox/bundle_manifest.py` and its README document schema 1:
  executable names, versions, absolute paths, byte counts and installed-byte
  SHA-256, source revision/archive hash, extensible harness list, and proxies
  explicitly recorded as in-process in aq-worker. Templates can advertise the
  harness list (SBX18) and copy `/opt/aq` as their final filesystem layer.

aq-git v0.2.0 was published and available at execution time; no fallback to the
missing `f3660a8` source revision was needed. Its release asset digest is
`3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2`.
Release: <https://github.com/ElectricJack/aq-git/releases/tag/v0.2.0>.
Node checksum source: <https://nodejs.org/dist/v22.16.0/SHASUMS256.txt>.
Other upstream URLs and hashes are in pins.json.

## Automated checks

All Cargo tests/checks use the existing AQ C1 pytest wrapper, preserving the
AQ test semaphore and `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`:

```sh
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","test","--locked","--workspace","runtime_bundle_lists_pinned_harness_versions"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test \
  docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
PYTHONDONTWRITEBYTECODE=1 aq test scripts/tests/test_package_sandbox.py -n 0
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","clippy","--locked","--all-targets","--","-D","warnings"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test \
  docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
cargo fmt --check
```

- Named workspace acceptance: **1 actual matching test passed**. Other
  workspace binaries reported filtered tests; these are not counted as passes.
  See `C3a-evidence/named.log`.
- Python packaging area: **6 passed**, including checksum and symlink refusal,
  private/untracked file exclusion, unchanged OS context across a harness
  change, floating image refusal and existing-output refusal.
  See `C3a-evidence/python-green.log`.
- Clippy and formatting: **passed**, `C3a-evidence/clippy.log` and `fmt.log`.
- TDD: the named test first failed on absent sandbox pins (`red.log`); the
  Python tests first failed on the absent generator (`python-red.log`). A
  follow-up fixture correction allows Git archive directory entries themselves
  while still rejecting every untracked/private file. It did not relax source
  file exclusion.

The generic profile's final `origin/dev` merge failed because this repository
has no `dev` branch. Its authoritative `origin/HEAD` points to `origin/main`.
`aq git pull --branch main --brief` and `git merge origin/main` succeeded
(already up to date). No auth or policy refusal was bypassed. Focused checks
are repeated after the final default-branch sync; full-suite CI remains the
integration owner's job, as directed by the worker prime.

## Image builds and layer experiment

Public input files were downloaded or reused and independently SHA-256
verified, without credentials. Contexts use the source commit above:

```sh
python3 scripts/package-sandbox.py --revision c2b17674e7f463f5cde5bcf120a331859b65ef58 \
  --inputs /tmp/c3a-public-inputs --output /tmp/c3a-final-contexts
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-os:c3a /tmp/c3a-final-contexts/os
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-runtime:c3a /tmp/c3a-final-contexts/runtime
```

The previous Codex 0.165.0 official archive was verified against
`be635d3486a5bd060d0fff582ae6d30160280c31344dd85719707fa920e187f0`.
Its extracted native executable hashes to
`c06a67fd8ac76508dc78bd99ce20c9f317b8d568beda59485f3f4134df9962d5`.
Only the Codex input/version/archive pins are changed in a temporary copy of
the pins. The programmatic `package(..., pins=temporary_pins)` interface
generates `/tmp/c3a-final-prior-contexts` at the same worker source revision;
the production CLI always reads the committed pins. The alternate harness is
used only for a layer experiment, never advertised as qualified.

`C3a-evidence/context-comparison.json` records that all four actual OS context
files are byte-identical before and after this real harness change.

Both runtime builds and both OS builds **passed**. The immutable local image
config IDs are:

| Artifact | Before (Codex 0.165.0) | After (Codex 0.166.0) |
| --- | --- | --- |
| OS | `sha256:876630cf2870b3af7f44620c0a9a40543c80a0c3511afa7edf9e76861ea7d063` | identical |
| Runtime | `sha256:b4ed980df0e7632c57d67b928fcb5554ab795884ba721d0ff4fbb472918d495e` | `sha256:e07bed781e32d30c17589d081b1f11b6aa896b60271731aa63d5a25daa32cca5` |

Two ephemeral local compositions use only these three Dockerfile instructions
(the runtime tag selects before/after). These are test artifacts, not the C3b
recipe interface or C3c production templates:

```dockerfile
FROM aq-sandbox-runtime:c3a AS runtime
FROM aq-sandbox-os:c3a
COPY --from=runtime /opt/aq /opt/aq
```

```sh
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-os:c3a-before /tmp/c3a-final-prior-contexts/os
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-runtime:c3a-before /tmp/c3a-final-prior-contexts/runtime
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-c3a:before /tmp/c3a-compose-before
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-c3a:after /tmp/c3a-compose-after
docker image inspect aq-sandbox-os:c3a-before aq-sandbox-os:c3a \
  aq-sandbox-runtime:c3a-before aq-sandbox-runtime:c3a \
  aq-sandbox-c3a:before aq-sandbox-c3a:after
PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3a-evidence/test_images.py -n 0
```

**The six OS layer DiffIDs are unchanged; only the seventh, final bundle
layer changes.** Before:
`sha256:ddadd591daa287e1d92f64836eeb388143e61dcb1fb86ae9dcc2e03b85314cac`.
After:
`sha256:d03110b34cff13c9396b98b2e9f35fdd8f4ea2fcc4a54807da33b02a59bc04f1`.
Each scratch runtime has exactly one layer. Runner, session host, aq-git and
private Node executable hashes are identical across the two installed
manifests; the harness version/digest changes as expected. The source metadata
also stays identical. Full inspect records and assertions are represented in
`C3a-evidence/layers.json`. These are Docker RootFS uncompressed layer
DiffIDs and local image config IDs, not registry publication digests.

Physical smoke: **2 tests passed**. Build logs are retained in
`C3a-evidence/*build.log`; only line endings and
trailing whitespace are normalized, with original log hashes in
`build-logs.json`. Physical smoke results are in `images.log` and the
observed installed-byte manifest is in `installed-manifest.json`. Checks use
an unprivileged agent UID, read-only root filesystem, temporary `/tmp`, no
network, no capabilities, and no new privileges for normal executable tests.
Loader inspection also runs without networking. They verify executable
versions/hashes/sizes, exact source metadata, the absolute private Node path
with a project PATH lacking Node, all binary loader dependencies, both OS
users, CA certificates, Git init/add/write-tree, and a private tmux session.

## Limits and review

Only linux/amd64 is supported. The private Node path is absolute and Codex
is native; future Node harness wrappers must select `/opt/aq/node/bin/node`.
Fixed inputs and locked dependencies do not establish bitwise reproducibility
of compiler output, OCI metadata or compressed layers. Record each published
artifact digest separately. Dated OS pins require explicit scheduled security
refreshes; publication/refresh automation belongs to C3c.

No production template recipes, assembly/finalize checks, credential scanner,
allowlist or GHCR publication were added (C3b/C3c). No runner privilege drop,
proxy behavior, aq-agent CLI or other harness implementation was changed.
The physical checks verify packaging and dynamic loader closure, not provider
execution, hostile-code isolation, or end-to-end sandbox lifecycle behavior.
No live credentials are supplied to image builds or checks.

C3a has no `review: adversarial` marker. The author reviewed the two image
definitions, exported context boundary, manifest shape, input checksums and
layer ownership against the plan's five Review Focus items. No independent
cross-family review is claimed. No architecture/spec files were modified and
no design-document follow-up was needed.
