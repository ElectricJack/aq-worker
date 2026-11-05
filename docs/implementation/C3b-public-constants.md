# C3b public constants qualification

Task `calm-delta-18.5.1`, source base `9a00f05`. This fixes the scanner refusal
blocking the real C3c Rust and Node seeds without changing recipe validation,
finalize, OCI verification, provenance, or the allowlist writer.

## Evidence and decision

The initial Rust archive contained 64 matching files with 62 distinct file
digests. Node added 12 distinct digests. Matches included GnuTLS self-test
private keys, token-shaped compiled strings, source assignments such as parser
tokens, and public documentation examples. GnuTLS contains complete published
test vectors, so accepting bare PEM syntax alone would not resolve this case.
Its [upstream self-tests](https://github.com/gnutls/gnutls/blob/3.8.3/lib/crypto-selftests-pk.c)
explain the embedded vectors; qualification verifies the exact Debian package
bytes rather than assuming a source version produces identical binaries.

`credential_scan.PUBLIC_FILE_CONSTANTS` contains only 76 exact SHA-256/size
pairs. `scan_content` hashes every original byte through EOF. A strict content
match can pass only when the complete file matches one of those reviewed
public files. The catalog is compiled into the embedded Python scanner. An
image, recipe, package name, path, extension, or ELF header cannot add entries.
Unknown versions, any changed byte, appended data, truncation, and concatenated
public files retain the strict refusal. This intentionally requires fresh
review when a public package update introduces new matches.

Private-key and provider-token recognition remains strict. Bundle manifest
capture never uses the catalog. Credential
paths, link aliases, image config/history, and environment remain independently
strict. Every inherited layer and deleted file still passes through the same
OCI verification and scan path. No archive or file-type exemption is added.

`C3b-public-constants-evidence/public-files.json` records each candidate file's
full digest/size, example location, finding kinds/count, upstream HTTPS archive,
pinned archive digest, and member name. These files were independently matched
against public Debian `.deb` files, Git 2.56.0, Rust 1.96.1, Codex 0.166.0,
Node 22.16.0/24.11.0 and pnpm 10.34.5 archives. The sole installation transform
is Git's configured `#!/usr/bin/python` shebang for `git-p4.py`.
No matched values or private-key example bodies are committed or printed.

## Verification

Environment: Linux amd64, Python 3.12.3, Rust 1.96.1, Docker/BuildKit's explicit
`default` context and existing `aq-c3c-qualification` builder. The C3c builder
uses a local registry mirror for its exact pinned C3a parent manifests.
Qualification edits only a disposable allowlist under
`/tmp/aq-c3b-public-qualification`; it does not publish to GHCR or change
production configuration. The existing builder is owned by C3c and retained.

Commands below run from the repository root. Logs are retained beside the
public-file manifest; full-suite testing belongs to integration.

```sh
PYTHONDONTWRITEBYTECODE=1 aq test scripts/tests/test_template_tool.py -k public_constants -n 0
AQ_TEMPLATE_BINARY="$PWD/target/debug/aq-worker" PYTHONDONTWRITEBYTECODE=1 \
  aq test scripts/tests/test_template_tool.py scripts/tests/test_package_sandbox.py -n 0
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","test","--locked","-p","aq-worker","--test","templates","--test","runtime_bundle","--bin","aq-worker"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
C1_CARGO_CWD="$PWD" \
  C1_CARGO_COMMAND='["cargo","clippy","--locked","--all-targets","--","-D","warnings"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s
cargo fmt --check
AQ_TEMPLATE_BINARY="$PWD/target/debug/aq-worker" \
  C3B_BUILD_CONFIG=/tmp/aq-c3c-calm-delta-18.5/builder.staging.json \
  C3B_PHYSICAL_ROOT=/tmp/aq-c3b-public-qualification \
  C3B_PUBLIC_CACHE=/tmp/aq-c3b-public-verification PYTHONDONTWRITEBYTECODE=1 \
  aq test docs/implementation/C3b-public-constants-evidence/verify_public_files.py \
    docs/implementation/C3b-public-constants-evidence/test_seed_admission.py -n 0 -s
```

The first regression run failed all three new scanner cases before the
catalog mechanism existed (`regression-red.log`); the implementation passed
all three (`regression-green.log`). Coverage checks exact public content,
same-length edits, appended tokens, truncation/concatenation, strict captured
metadata, credential paths, deleted layers, streaming boundaries, and an
unchanged existing allowlist after a modified public file refuses.

Synchronizing main also made adjacent public Serde strings reachable in the
worker binary: `persistent-disk-during-attemptbackend_idstruct ` (including
the trailing space). The `sk-` within `disk-` plus the neighboring labels
matched the provider-token pattern. An exact fixed-context lookaround now
excludes only that occurrence in the `sk-` branch. It consumes no bytes and
leaves URL, assignment and other token branches independent. Standalone keys,
credential assignments using the same text, altered/suffixed contexts and
nearby real tokens still refuse. `metadata-red.log` and `metadata-green.log`
record the targeted regression. This avoids a recursive fingerprint of the
worker binary that embeds its own scanner.

A later build placed `struct SandboxIdentity` after the label instead, so the
fixed neighbor no longer matched and the embedded CLI scan refused
(bright-quest-86). The neighbor is codegen/linker layout, not content. The
exception is now anchored to the exact label `persistent-disk-during-attempt`
alone; it remains zero-width and limited to the `sk-` branch. Provider keys
appended directly to the label (`sk-proj-`, `sk-ant-`, `ghp_`, `AKIA`), the
label without its `persistent-di` prefix, altered spellings and truncations
still refuse.
`capability-label-root-cause.log` replays the scanner's chunking over the
failing binary before and after the change. `capability-label-templates-red.log`
and `capability-label-templates-green.log` hold the focused `templates` target.
`capability-label-python-green.log` holds the template-tool suite, and
`capability-label-workspace.log` holds the locked `--no-fail-fast` workspace run
(923 passed, 99 ignored). Its one failure was `driver`
`post_stop_observation_outage_preserves_and_replays_before_report`, which hit
EAGAIN (os error 11) spawning the CLI under host load. That target passed 5/5
when rerun alone.

The Rust area runs both existing named acceptance cases,
`finalize_refuses_template_missing_tmux_or_users` and
`credential_scan_fails_build_with_auth_json`, as well as standalone embedded
CLI build/scan and runtime bundle checks. Public-byte reproduction verifies
the checked-in catalog against pinned downloaded archives, without trusting
the candidate images.

Physical qualification invokes the rebuilt installed worker's existing
`template build` command with the exact recorded C3c seed recipes. It requires
the original Rust manifest digest
`sha256:4c9c20007445c3a207f31787e635ec753e64b13f22300cd90030784f7862a1c5`
and Node manifest digest
`sha256:5e0e668ad48da42b5d46f03e971123bc9552c5721d14a147096a7250d2305710`,
and validates both generated entries in the disposable allowlist. Final
results and local output identities are recorded below.

`git merge origin/dev` refused because that ref does not exist (`dev-merge.log`).
The remote default is `origin/main`; final synchronization uses guarded
`aq git pull --branch main`. No PR or independent default-branch delivery is
performed by this task.

## Observed results

- Final Python area: **25 passed**, `python-area-synced-green.log`.
- Final synced Rust area: **8 passed**, `rust-area-synced-green.log`, including
  both named acceptance tests, the installed CLI outside its source directory,
  runtime bundle, and four worker CLI unit tests. The initial area run had six
  tests before main introduced two additional CLI tests.
- Initial `cargo clippy --locked --all-targets -- -D warnings` and formatting
  passed (`clippy.log`, `fmt.log`).
- Final targeted clippy and formatting passed (`clippy-synced-green.log`,
  `fmt-synced-green.log`). The targeted clippy command replaces
  `--all-targets` with `-p aq-worker --test templates --test runtime_bundle
  --bin aq-worker`, leaving `--locked -- -D warnings` unchanged, after the
  main sync as required by the task's validation scope.
- Independent public-byte verification: **1 passed**, all 74 fingerprints
  reproduced (`public-verification-final.log`).
- Final physical seed admission: **1 passed** in 217.07 seconds, both original
  manifest digests preserved (`physical-admission-final.log`). Final entries,
  policies and archive checksums are in `admission-final.json`; outputs remain
  at `/tmp/aq-c3b-public-qualification/admission-zzgn2__x`.
- Physical seed admission: **1 passed**, both original manifest digests
  preserved, both entries generated through `template build`. The retained
  combined run (`physical-admission.log`) reports **1 failed, 1 passed**:
  its verification helper initially assumed Git's shebang was `python3`.
  The public evidence had correctly recorded `/usr/bin/python`; the helper
  was corrected and its independent rerun passed. The physical admission test
  itself passed, and no scanner or catalog change was needed for that correction.

The final scanner's physical replay uses the same environment shown above,
with `aq test docs/implementation/C3b-public-constants-evidence/test_seed_admission.py
-n 0 -s` alone; public-byte verification already passed independently. This
replay follows the final compiled-metadata fix and retains its own log.

The physical outputs remain at
`/tmp/aq-c3b-public-qualification/admission-blc2t8l_`. The checked-in
`admission.json` retains both generated entries and archive checksums;
`source-state.json` identifies the scanner bytes and synchronized main.
Each entry advertises Codex 0.166.0 and the unchanged
`aq-finalize-v1`/`aq-credential-scan-v1` policies. OCI archive checksums differ
from the earlier C3c exports, while their image manifest digests are identical.
The generated GHCR image references are metadata; this task performs no
registry publication.

The guarded main sync fast-forwarded to
`981682178b4ed8254497637af163e70da30aac5f` without conflicts. Its upstream
changes do not touch this scanner, its packaging, or its Python tests. A final
targeted Rust CLI/runtime-bundle run and targeted clippy run validate the
installed binary after synchronization, with final logs recorded alongside
the initial area checks. The first synced Rust run caught the public Serde
label false positive described above (`rust-area-final.log`); it is retained
as regression evidence. `cargo fmt --check` also passed after sync
(`fmt-final.log`).

## Claude Code 2.1.318 (calm-delta-18.9.3)

The runtime bundle now ships the Claude Code native executable at
`opt/aq/harnesses/claude/claude`, which the strict scanner refuses (178
token/assignment matches, all compiled JavaScript: concatenated CSS and skill
identifiers, PEM-header regexes and property assignments; no credential
values). One exact entry was added, `708c77b2…f0bd` / 337814032 bytes, the
`package/claude` member of the public npm tarball
`@anthropic-ai/claude-code-linux-x64@2.1.318` (archive `2a4d5e3b…554f`, whose
sha512 equals the registry `integrity`). The same bytes were independently
matched against a native-installer copy of 2.1.318. `verify_public_files.py`
covers the record; one appended byte still refuses.

## OpenCode 1.18.35 (azure-horizon-22.6, I3)

The runtime bundle now ships the OpenCode native executable at
`opt/aq/harnesses/opencode/opencode`, which the strict scanner refuses (205
token/assignment matches, all compiled JavaScript and embedded provider
metadata such as environment-variable names and property assignments; no
credential values). One exact entry was added, `77b2cfe4…24fa` / 185690240
bytes, the `package/bin/opencode` member of the public npm tarball
`opencode-linux-x64@1.18.35` (archive `e5c8997d…6133`, whose sha512 equals
the registry `integrity`). The same bytes were matched against an
npm-installed `opencode-ai@1.18.35` host copy. `verify_public_files.py`
covers the record.

## Limits

This remains conservative pattern/path detection. A catalog entry proves that
its exact bytes are public, not that public software is trustworthy or that
an image cannot contain unknown secret formats. Package updates fail closed
when their new content matches. Credentials must never be build inputs.
Public registry publication and the C3c GHCR authorization blocker are outside
this scanner task. No architecture/spec files are modified.
