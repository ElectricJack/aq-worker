# C3b — Template build, provenance and credential gates

Task `calm-delta-18.4`, based on `61084a6cd4648bcb4c7307ee37eeac9b418ba21b`.
Initial implementation source commit: `04388eeb8a8fe2b7fd293988ab5f912b9d5b0c68`. Evidence-only
updates follow without rewriting reviewed source history. Review fixes are in
`d6a0e8e4ddf3b3b9ffb74016e97730098e8f5839`; the final metadata fix is
`2543b3223b541e1e479694bb6fdf85bed5a35dcc`.

Delivered interface: `aq-worker template build <recipe> --build-config <file>
--output <new-directory>` and `aq-worker template scan <oci-archive>`.
The installed worker embeds Python packaging/scanner tools. Recipe, explicit
build-node, OCI scan, provenance, allowlist and C7/C8 materialization contracts
are documented in `packaging/sandbox/templates.md`. No aq-server files or
architecture/spec documents are changed; no design-doc follow-up is needed.

Environment: Linux x86_64, Python 3.12.3, rustc 1.96.1, Docker Engine 29.3.1/BuildKit on the
explicit local `default` context and `aq-c3b-qualification` docker-container builder. Cargo commands use the existing
C1 pytest wrapper through `aq test`, with CARGO_BUILD_JOBS=2 and
RUST_TEST_THREADS=2. Tests are scoped to the changed area as required by prime;
full-suite CI remains the integration owner's responsibility.

Initial implementation commands and results (historical):

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
docker buildx create --name aq-c3b-qualification --driver docker-container --bootstrap default
PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3b-evidence/test_physical_oci.py -n 0 -s
docker buildx rm aq-c3b-qualification
```

- Python area: 17 passed (`C3b-evidence/python-area.log`).
- Rust templates acceptance and embedded CLI: 3 passed (`cli-green.log`).
- Rust area: 6 passed (2 worker CLI unit tests, 1 runtime-bundle test, 3 template tests; `rust-area.log`).
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

Final sync: `git merge origin/dev` refused because that ref does not exist.
`origin/HEAD` is `origin/main`; `aq git pull --branch main --brief` merged
main without conflicts as `9ef1749`. Its new protocol/aq-admission workspace
member required a further explicit source-export entry; the regression first
failed in `protocol-export-red.log`. No unrelated protocol files are exported.
The qualification builder was removed after the 2/2 physical pass. Its
BuildKit image was `moby/buildkit@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea`.


Final continuation verification uses source `2543b3223b541e1e479694bb6fdf85bed5a35dcc`
on the main-synced branch (`4fe3f81` merged `303c5f3`). The requested
`origin/dev` ref is absent; guarded `aq git pull --branch main --json`
succeeded and `git merge origin/dev` recorded the absent-ref refusal in
`dev-merge-continuation.log`. Published and reviewed source history is preserved.

The recovered installed-binary regression reproduced red in
`metadata-continuation-red.log`, `python-continuation.log` and
`rust-continuation.log`: the public mapper format label was classified as
credential content. The fix permits only the exact JSON metadata pair
`"credentials":"private-codex-chatgpt-auth-json"`; arbitrary JSON credential
values, suffix variants and shell assignments still refuse. The two area
suites pass with the fix and the rebuilt embedded worker:

- Python area: **20 passed**, `python-continuation-green.log`.
- Rust area: **6 passed** (2 CLI unit, 1 runtime bundle, 3 template integration),
  `rust-continuation-green.log`.
- Standalone committed source archive: **1 passed**, `source-export-final.log`.
  Its fixture generates public input bytes and pinned test hashes locally;
  it needs no external `/tmp` assets.
- Formatting: `cargo fmt --check` passed, `fmt-final.log` (empty success output).
- Physical smoke: **2 passed**, `physical-continuation.log`; manifest digest
  `sha256:e0e102e1cf634ffaed861894d25f1abcdc7dd2cd022be0138880aa1a0bf9f37f`.
  The dedicated builder used the pinned BuildKit image above and was removed
  (`build-node-continuation.log`, `builder-cleanup-continuation.log`).

The final area commands are the same commands shown above. Archive verification:

```sh
PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C3b-evidence/test_source_export.py -n 0 -s
```

Fresh final review of `4fe3f81..2543b32` read the public mapper fixture and
surrounding assembly/scanner/allowlist integration. The reviewer reported no
Critical, Important or Minor findings, and independently probed 3 metadata
acceptance cases, 7 refusal cases and 48 streaming boundary placements. C3b has
no required cross-family adversarial marker. Earlier recovered logs retain
intermediate failures as TDD/review evidence; the continuation logs above are
the final area results. The production-publication and optional-universal-base
limits stated above still apply.


Exact acceptance commands were run through the C1 resource-controlled wrapper
with `C1_CARGO_CWD="$PWD"`, `PYTHONDONTWRITEBYTECODE=1`, `-n 0 -s`, and each
of these `C1_CARGO_COMMAND` arrays:

```sh
["cargo","test","--locked","--workspace","finalize_refuses_template_missing_tmux_or_users"]
["cargo","test","--locked","--workspace","credential_scan_fails_build_with_auth_json"]
```

Both passed on final source. `named-finalize-continuation.log:667` reports
`test finalize_refuses_template_missing_tmux_or_users ... ok`;
`named-credential-continuation.log:665` reports
`test credential_scan_fails_build_with_auth_json ... ok`. Other workspace
binaries report zero matching tests as expected; each required named test
executed once. Each resource wrapper also reports one passing wrapper test.


Final `cargo clippy --locked --all-targets -- -D warnings` also passed through
the wrapper (`clippy-continuation.log`). All required named tests, related
area tests, archive smoke, physical smoke, fmt and clippy passed; no acceptance
item is waived. Final evidence changes do not alter the reviewed runtime code.
