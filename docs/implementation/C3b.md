# C3b — Template build, provenance and credential gates

Task `calm-delta-18.4`, based on `61084a6cd4648bcb4c7307ee37eeac9b418ba21b`.
Implementation source commit: pending final source commit. Evidence-only
updates follow without rewriting reviewed source history.

Delivered interface: `aq-worker template build <recipe> --build-config <file>
--output <new-directory>` and `aq-worker template scan <oci-archive>`.
The installed worker embeds Python packaging/scanner tools. Recipe, explicit
build-node, OCI scan, provenance, allowlist and C7/C8 materialization contracts
are documented in `packaging/sandbox/templates.md`. No aq-server files or
architecture/spec documents are changed; no design-doc follow-up is needed.

Environment: Linux x86_64, Python 3.12.3, rustc 1.96.1, Docker/BuildKit on the
explicit local `default` context and `aq-c3b-qualification` docker-container builder. Cargo commands use the existing
C1 pytest wrapper through `aq test`, with CARGO_BUILD_JOBS=2 and
RUST_TEST_THREADS=2. Tests are scoped to the changed area as required by prime;
full-suite CI remains the integration owner's responsibility.

Commands and current results:

```sh
AQ_TEMPLATE_BINARY="$PWD/target/debug/aq-worker" PYTHONDONTWRITEBYTECODE=1 \
  aq test scripts/tests/test_template_tool.py scripts/tests/test_package_sandbox.py -n 0
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","test","--locked","-p","aq-worker","--test","templates","--test","runtime_bundle","--bin","aq-worker"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","clippy","--locked","--all-targets","--","-D","warnings"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
cargo fmt --check
PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3b-evidence/test_physical_oci.py -n 0 -s
```

- Python area: 17 passed (`C3b-evidence/python-area.log`).
- Rust templates acceptance and embedded CLI: 3 passed (`cli-green.log`).
- Rust area: 8 passed (4 worker CLI unit tests, 1 runtime-bundle test, 3 template tests; `rust-area.log`).
- Clippy and `cargo fmt --check`: passed (`clippy.log`, `fmt.log`).
- Physical smoke: 2 passed (`physical-container.log`): a real gzip OCI export scanned successfully, and finalize passed on C3a's existing image. The smoke manifest digest is `sha256:dccbbf507f4a6d3e0366f9f3464b569a91a5f8beadbcc6848aac06ea46494ec4`; its synthetic bundle content is not production runtime qualification.
- The default Docker driver refused OCI export (`physical.log`); a separate named docker-container builder succeeded. No partial/export fallback was used. Exact named workspace filters are recorded below when completed.
- Initial `cargo fmt --all -- --check` could not traverse a vendored package's
  outer-workspace association. Required `cargo fmt --check` is the check used;
  no vendor or workspace definitions are changed for this tooling issue.

TDD: `python-red.log` records five failed tests on absent tools;
`python-task1.log` records five passing tests. `build-red.log` records absent
build/config APIs; `build-green.log` records 8 passing tests. `cli-red.log`
records the missing CLI subcommand before implementation and `cli-green.log`
records its pass outside the source directory. `source-export-red.log` records
missing embedded Python inputs in the C3a runtime archive; the area suite
checks the fixed explicit export list. `secret-shapes-red.log` records URL,
JWT and lowercase API-key cases first failing and the area suite passing.
The initial named-finalize Cargo run completed after implementation; its
log `finalize-initial.log` contains that initial passing run and it is **not** red evidence.

The test Docker boundary double checks exact context/builder/platform/export
arguments, refuses forwarded provider keys or extra context files, and exports
real hand-built OCI archives for the real scanner and allowlist writer. It
is not claimed as a physical toolchain build. Failed BuildKit and credential
scan cases leave the allowlist untouched. Descriptor substitution, deleted
credentials, environment/content/path/link secrets, manifest whiteouts,
compressed layers, malformed recipes, public prefetch and parent resolution
are covered.

Limits: no production toolchain image is published (C3c), no launch enforcement
is added (C2/C7), and no snapshot backend is implemented (C7/C8). Inputs and
build infrastructure are trusted operator configuration. Credential scanning
is conservative pattern/path detection, not mathematical proof of absence of
all secrets. Universal-base bootstrap requires an apt-compatible amd64 image;
no upstream universal image is physically qualified here. Registry visibility
is an explicit operator ACL assertion. The allowlist/provenance are unsigned
configuration; protect them from attempt writers. No hostile-code isolation
or real provider execution is claimed.
