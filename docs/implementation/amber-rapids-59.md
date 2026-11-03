# amber-rapids-59: single-use Cargo settings in sandbox templates

Single-use sandboxes gain nothing from Cargo's incremental cache. In v1, one
slot held 37 GB under `debug/incremental`. Every C3b-generated template now
carries this line in its image environment:

```dockerfile
ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
```

## Change

- `packaging/sandbox/template_tool.py`: `CARGO_ENV` is emitted once by
  `dockerfile()`. It sits after the install steps and prefetch, and before
  `WORKDIR` and the final runtime copy, so install-step cache layers are
  unchanged. It applies to OS, allowlisted and universal parents alike.
- `packaging/sandbox/templates.md`: documents the variables and their limits.
- `scripts/tests/test_template_tool.py`:
  `test_dockerfile_sets_single_use_cargo_environment_after_installs`.
  Red before the change, green after it.

Only these three keys are overridden. Other repository profile settings
(`opt-level`, `debug` for other profiles, `codegen-units` and so on) stand.
The aq-sandbox-runtime image is scratch and copied only as `/opt/aq`, so it
cannot carry ENV. The template layer is the only image-level place to set it.
The image recipe schema is unchanged, and the recipe digests do not change.

## Verification

Environment: Linux x86_64/WSL2, Docker 29.3.1, the existing
`aq-c3c-qualification` BuildKit builder, and the C3c loopback mirror
`aq-c3c-registry` at `127.0.0.1:5138`.

```sh
PYTHONDONTWRITEBYTECODE=1 aq test scripts/tests/test_template_tool.py -k cargo_environment -n 0   # red, then green
cargo build --locked -p aq-worker --bin aq-worker
AQ_TEMPLATE_BINARY="$PWD/target/debug/aq-worker" PYTHONDONTWRITEBYTECODE=1 \
  aq test scripts/tests/test_template_tool.py -n 0                 # 20 passed
C1_CARGO_CWD="$PWD" C1_CARGO_COMMAND='["cargo","test","--locked","-p","aq-worker","--test","templates"]' \
  PYTHONDONTWRITEBYTECODE=1 aq test docs/implementation/C1-evidence/test_cargo_check.py -n 0 -s  # 3 passed
```

Logs: `amber-rapids-59-evidence/{python-template-tool,rust-templates,cargo-build}.log`.

### Rebuilt templates (staging admission, not production)

Both C3c seed recipes (the same bytes as
`C3b-public-constants-evidence/{rust,node}.recipe.json` and
`aq/calm-delta-18.5:packaging/sandbox/recipes/*.json`) were rebuilt with the
installed `aq-worker template build`. Each build used the C3c builder config,
with builder identity `amber-rapids-59-staging` and a disposable allowlist
outside Git. Both passed `aq-finalize-v1` and `aq-credential-scan-v1`.

| template | image manifest digest |
| --- | --- |
| rust@1 | `sha256:cfdf741b7ea51745fbf899c58378472e57ee10e75add1bdf1dbcde78a9cce643` |
| node@1 | `sha256:3351026cfe4b15613146174e2bb519fae3454d61613ae7702f33a8f3406a3e99` |

These digests replace the earlier rust
`sha256:4c9c2000…` and node `sha256:5e0e668a…` digests, because the added
ENV changes the image config. The full entries and archive checksums are in
`amber-rapids-59-evidence/staging-admission.json`. The rendered Dockerfile is
in `amber-rapids-59-evidence/rust.Dockerfile`.

Each archive was copied into the loopback mirror using the C3c pinned Skopeo
(`copy --preserve-digests`). That produced
`127.0.0.1:5138/electricjack/aq-template-staging@<digest>`. A raw-manifest
SHA-256 readback matched both digests.

### Launched template

`amber-rapids-59-evidence/launch-env.log`: each image was launched with
`--network none --cap-drop ALL --security-opt no-new-privileges --user agent`.

- `env | grep CARGO_` printed all three settings in both the rust and node
  templates.
- In the rust template, `cargo new`, `cargo build` and `cargo test` in a fresh
  crate left `target/debug/incremental` empty. `cargo -v` showed
  `-C debuginfo=line-tables-only` and no `-C incremental` for both the dev and
  test builds.

## Rust template sizing for the owner's boat (2 vCPU / 4 GB)

The owner later asked for two more Rust-only defaults: one build job, and a
low-memory linker.

- **One build job.** The recipe schema has no environment field, and the
  shared `CARGO_ENV` line covers every template, so the default lives in the
  Rust recipe instead. `amber-rapids-59-evidence/rust.recipe.json` is the C3c
  seed recipe plus one install step that writes `/.cargo/config.toml` with
  `[build] jobs = 1`. Cargo reads this file from every working directory under
  `/`, at the lowest priority:
  - repository `.cargo/config.toml` files, `CARGO_BUILD_JOBS` and `-j` all
    still win;
  - unlike image ENV, it still applies when the harness clears the tool
    environment (see `swift-vault-73`).

  **C3c must adopt this recipe** in
  `aq/calm-delta-18.5:packaging/sandbox/recipes/rust.json`, replacing the
  seed copy, before it publishes.
- **Low-memory linker.** No change was needed. Rust 1.96.1 already links
  `x86_64-unknown-linux-gnu` with its bundled `rust-lld`: `--print link-args`
  in the template shows `-B…/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld`
  and `-fuse-ld=lld`. mold is not in the image. Adding an explicit
  `RUSTFLAGS`/`build.rustflags` linker flag would be redundant. It would also
  replace (with `RUSTFLAGS`) or merge into (with config) the repository's own
  rustflags, so it was left out.

Rebuilt rust@1 image (staging admission, fresh disposable allowlist):
`sha256:42ac1990e864112c47831ae8bed7f45b004a7291d6828f158083f6e8eff48a1c`.
It was copied with `--preserve-digests` to the loopback mirror, and a
raw-manifest readback matched the digest. It supersedes the rust digest in
the table above. The node digest is unchanged.

Its recipe digest is
`sha256:4fe5c0745f7866f1ec486eb87e326462870aee5c48c9c2c3332b1074c9a90401`.

### Peak memory: targeted aq-server test build

The run used a `git archive` of aq-server `283e4cc7`. The rebuilt rust@1
image was launched with `--memory 4g --memory-swap 4g --cpus 2 --cap-drop ALL
--security-opt no-new-privileges --user agent`, which is the boat size. The
script ran `cargo fetch --locked`, then
`cargo test --locked -p aq-server --test config`, measured from a cold
target dir. Script and log: `amber-rapids-59-evidence/{measure.sh,aq-server-peak-rss.log}`.

| measure | value |
| --- | --- |
| cgroup `memory.peak` (includes page cache) | 2,925,645,824 B (2.72 GiB) |
| largest single process max RSS (`getrusage` children) | 2,380,116 KiB (2.27 GiB) |
| `memory.events` max / oom / oom_kill | 0 / 0 / 0 |
| result | 13 passed; build finished in 7m 16s |

The peak stays under the 4 GB limit with about 1.3 GiB to spare. Inside the
container, `nproc` reports the host's 24 CPUs; the cgroup quota
(`cpu.max 200000 100000`) is what limits it to 2 vCPU. The jobs default
therefore comes from `/.cargo/config.toml`, not from CPU detection.

## Limits

- **GHCR publication not performed: needs GHCR token.** `GHCR_TOKEN`,
  `GH_TOKEN` and `GITHUB_TOKEN` are absent; only their presence was checked.
  The C3c publish flow (`aq/calm-delta-18.5:docs/implementation/C3c.md`,
  "Prepared publication commands") is unmerged and still gated on that token.
  When it runs, it must use the digests above, and the rebuilt archives must
  be republished. No production allowlist was changed.
- The Codex harness runs tool commands with
  `shell_environment_policy.inherit="none"` and only `PATH=/usr/bin:/bin`
  (`crates/aq-codex-mapper/src/lib.rs:137-144`). As a result, agent tool
  commands do not yet inherit the image ENV. Follow-up: `swift-vault-73`.
