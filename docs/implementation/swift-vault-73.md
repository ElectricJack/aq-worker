# Fixed Codex tool environment

The shared exec/interactive mapper explicitly sets:

| Variable | Value |
| --- | --- |
| `PATH` | `/usr/local/bin:/usr/bin:/bin` |
| `CARGO_INCREMENTAL` | `0` |
| `CARGO_PROFILE_DEV_DEBUG` | `line-tables-only` |
| `CARGO_PROFILE_TEST_DEBUG` | `line-tables-only` |

These are trusted literals. The mapper does not read image, operator or attempt
environment variables. `inherit="none"`, disabled login shells, disabled shell
profiles and disabled shell snapshots remain in the recipe. `/usr/local/bin`
contains the seed recipes' Rust and Node toolchain symlinks. The local-result
variant retains these defaults and adds only its two explicit channel values;
its recipe declaration now describes the updated environment too.

Both complete worker command goldens and the exact four-entry environment
assertion were updated. The local-result test checks its six-entry environment.
OpenAI's [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
documents the separate inheritance baseline and explicit `set` map.

## Physical acceptance

The [opt-in probe](swift-vault-73-evidence/test_template_environment.py) launches
an existing C3b Rust template, runs its pinned Codex 0.166.0 with the mapper's
base command golden, and supplies one real shell tool call through an offline
scripted Responses endpoint. Only fixture model/provider settings, workspace
location and the Git-repository preflight are adapted. No provider credentials
or external network are used. Docker has a read-only root, UID/GID 10002,
no capabilities, no-new-privileges, the repository's narrow Codex seccomp
profile, private tmpfs state, one CPU, 1 GiB memory and 128 PIDs.

The template came from the previously built local archive
`/tmp/aq-c3b-public-qualification/admission-zzgn2__x/rust/image.oci.tar`,
OCI manifest `sha256:4c9c20007445c3a207f31787e635ec753e64b13f22300cd90030784f7862a1c5`.
It was converted offline with the installed Skopeo image to a Docker archive
and loaded as `aq-swift-vault-73:rust`, config/image ID
`sha256:0e2d4bc96d665806dacb9a3641f0f1a790aec9b4546d667997d2956c211e2171`.
No image build or pull was needed. This older template predates the originating
task's Cargo ENV change. The probe deliberately sets the Codex parent Cargo
values to `1`, `2`, `2` and adds an ambient sentinel, verifying that the fixed
tool policy works independently of the image's environment.

The actual tool output was:

```text
0
line-tables-only
line-tables-only
/usr/local/bin/cargo
cargo 1.96.1 (356927216 2026-06-26)
```

The shell also asserted that the parent sentinel was absent. The probe requires
the output to reach the provider as a tool result, as well as checking a file
written by the tool. This validates Codex execution in a launched template;
it does not claim a new worker/runtime image publication.

## Verification

Run from the repository root; the existing C1 wrapper enforces Cargo build jobs
and Rust test threads at two through AQ's test semaphore:

```sh
C1_CARGO_COMMAND='["cargo","test","--locked","--offline","-p","aq-worker","--lib","native::adapter::codex::tests"]' \
  C1_CARGO_CWD="$PWD" PYTHONDONTWRITEBYTECODE=1 \
  aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -q -s
C1_CARGO_COMMAND='["cargo","test","--locked","--offline","-p","aq-codex-mapper","--lib"]' \
  C1_CARGO_CWD="$PWD" PYTHONDONTWRITEBYTECODE=1 \
  aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -q -s
TEMPLATE_ENV_IMAGE=aq-swift-vault-73:rust PYTHONDONTWRITEBYTECODE=1 \
  aq test docs/implementation/swift-vault-73-evidence/test_template_environment.py -n 0 -q -s
rustfmt --edition 2024 --check crates/aq-codex-mapper/src/lib.rs crates/aq-worker/src/native/adapter/codex/tests.rs
git diff --check
```

Mapper: 9 passed. Physical probe: 1 passed. Adapter: 11 passed.
Logs: [mapper](swift-vault-73-evidence/mapper-tests.log),
[physical probe](swift-vault-73-evidence/template-environment.log),
[adapter](swift-vault-73-evidence/adapter-tests.log).
Direct rustfmt and whitespace checks passed. `cargo fmt --all --check` could
not resolve a pre-existing vendored package's workspace in this nested checkout;
the direct rustfmt command checks the changed Rust files and their modules.
`git merge origin/main` reported already up to date before final checks.
