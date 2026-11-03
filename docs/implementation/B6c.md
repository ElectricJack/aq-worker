# B6c — Runner hydration from retention and per-sandbox channels

Task: `noble-summit-37.9`. Plan: commit `b001f78` (TDD plan). Implementation: `fa3de67`. Final revision: the commit containing this document. The exact published OID is recorded in the AQ task close.

## Delivered

### Hydration (0005 §4 step 4, SBX17)

`crates/aq-worker/src/native/runner/sandbox/hydrate.rs` adds `hydrate(&RunnerHttp, &SourceBinding, &GitClient) -> HydratedSandbox`. It works as follows:

1. It validates the pinned `SourceBinding`. It then reads the retained `TransferManifest` from `/v1/worker/profiled/source/{attempt}/status` under the delegated permit (`RunnerHttp::source_status`). It refuses unless the manifest's source and checkpoint equal the pinned revision.
2. It fetches the manifest and the bundle members in 256 KiB ranges from `/v1/worker/profiled/source/{attempt}/{member}` (`RunnerHttp::source_chunk`). Each response is checked for:
   - the source API version and the octet-stream body type;
   - the `aq-source-transfer` metadata, which must equal the status manifest;
   - a single `aq-source-offset`, which must equal the requested offset;
   - the exact byte count.

   The bytes go into the existing private `Staging` area under the aq-git root.
3. It verifies the staged bytes' sizes and SHA-256 digests against the descriptor before aq-git sees them (`Staging::verify`). It then publishes, registers the binding if needed, imports the artifact and creates the workspace through aq-git.
4. It re-verifies the staged manifest digest. It then validates the import, create, retention and workspace inspection results with the same checks as preparation hydration. `validate_current` now delegates to the shared `validate_retained`.

The runner receives no Git remote URL or credential, and no code path contacts a remote. The acceptance test makes the source repository's `origin` point at `https://git.invalid` and asserts that the hydrated workspace has no remote at all.

### Channels (SBX16)

`crates/aq-worker/src/native/runner/sandbox/channels.rs` adds `SandboxChannels`. It is built from the sandbox's `RunnerHttp` and checks both the terminal-poll and observation scopes up front. Its properties:

- **Separate pool.** It uses its own connection pool (`RunnerHttp::channel_pool`) that shares authority-loss state with the renewal client. Terminal traffic never consumes the renewal or cancellation pool.
- **B1 identity.** The B1 `PermitBinding` (attempt, generation, sandbox, node/boot, channel) is used on every request with the permit bearer. The relay peer is `Remote{node_id, boot_id}` from the binding.
- **Idle sends nothing.** `poll_terminal` returns `None` without I/O when there are no subscriptions or controllers. `run_terminal` waits on a demand watch while idle.
- **One poll per sandbox.** The cursor mutex is held for the length of a poll. A second concurrent poll is refused, not queued.
- **Bounded hold and frames.** The hold is 20 000 ms, validated by `RelayPoll::validate`. Request and response frames are each bounded at 64 KiB through `RunnerHttp::post_bounded`. Replies are checked with `wire::decode`, and their peer and cursor must be monotonic.
- **Observations.** `upload_observations` posts on the same per-sandbox channel and permit.
- **Server-side aggregates.** Per-node aggregate limits are server-side and are not implemented here.

### Supporting changes

- **`sandbox/auth.rs`:**
  - `pool()` builds the shared client settings: no proxy, no redirects, no retries, 3 s connect and 22 s total timeout.
  - `channel_pool` and `launch_binding` are new.
  - `post` now delegates to `post_bounded`.
  - `source_get`, `source_status` and `source_chunk` are new.
  - The response path is split into `status`, which keeps the refusal → authority-loss mapping, and `read`, which applies the size cap and the post-read authority check.
- **Visibility only.** `native/http.rs` and `native/http/source.rs` make their header and transfer validators `pub(crate)`. `native/hydration/files.rs` does the same for `Staging`, and `native/preparation.rs` exposes the `hydration` module. Behaviour is unchanged; the existing source and hydration tests pass.

## Limits

- **B2 server routes are not published.** Loopback axum fixture peers stand in for the central source export and the terminal relay. They speak the headers and envelopes already pinned in this repository. Production server compatibility is not verified.
- **Pinned aq-git.** No runtime-bundle manifest format for pinning aq-git exists yet, so the plan's `start_pinned_git` was dropped. `hydrate` takes a `GitClient` and documents that it must come from the pin-enforcing `GitService::start_with_git`. The test uses a fake aq-git socket server backed by real `git`.
- **Not wired into launch.** `hydrate` and `SandboxChannels` are library APIs with tests. Wiring them into the production `sandbox-run` launch is blocked on the same B2 codec limit recorded in B6a2.
- **Out of scope, per task:** cold remote fetch, H1 readers and store, the H7 renderer, H5 hibernation, and per-node aggregate enforcement.

## Verification

- **Environment:** Linux 6.18.33.2-microsoft-standard-WSL2, rustc 1.96.1 (31fca3adb 2026-06-26), cargo 1.96.1 (356927216 2026-06-26).
- **Cargo settings:** the committed lockfile, `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`.
- **Test conditions:** peers are loopback only. Tests use no real sleeps; held polls are gated by a semaphore, and deadlines use the runtime-scoped `TestClock`.

**RED:** `B6c-evidence/red-hydrate.log` shows the tests failing to compile against the missing `hydrate` and `SandboxChannels`, with rc 101.

**GREEN:** these results are at `fa3de67`.

| Exact command | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked --workspace hydrated_workspace_matches_retained_revision` | 1 matching test passed | `B6c-evidence/hydrated_workspace_matches_retained_revision.log` |
| `cargo test --locked --workspace idle_sandbox_holds_no_terminal_poll` | 1 matching test passed | `B6c-evidence/idle_sandbox_holds_no_terminal_poll.log` |
| `cargo test --locked --workspace hydration_refuses_retained_bundle_digest_mismatch` | 1 matching test passed | `B6c-evidence/hydration_refuses_retained_bundle_digest_mismatch.log` |
| `cargo fmt --check -p aq-worker` | passed | `B6c-evidence/fmt.log` |
| `cargo clippy --locked -p aq-worker --all-targets -- -D warnings` | passed | `B6c-evidence/clippy.log` |

Affected-area runs during implementation:

- `native::runner::sandbox`, together with the hydration file tests: 16 passed.
- The `hydration_scenarios` filter: 68 passed and 17 ignored.

No full test suite is claimed, in line with the owner's velocity rule.

## Acceptance map

- **`hydrate_tests.rs:390–441` — `hydrated_workspace_matches_retained_revision`.**
  - Workspace state: HEAD equals the retained commit, the file content matches, the tree is clean, and the workspace has no Git remote.
  - Manifests: the returned manifests equal the retained descriptors.
  - Requests: every request goes to the attempt's source prefix with the permit bearer.
  - aq-git: the exact op sequence is `capabilities`, `repo.show`, `repo.register`, `artifact.import`, `workspace.create`, `retention.inspect`, `workspace.inspect`.
- **`hydrate_tests.rs:443–468` — digest mismatch.** It is refused with "bundle digest" before any import or create.
- **`channels_tests.rs:62–145` — `idle_sandbox_holds_no_terminal_poll`.**
  - Idle: zero requests and no poll held.
  - With demand: exactly one held poll on `/v1/terminal-relay/poll` with the bearer, `hold_ms` 20000 and the B1 peer. A concurrent second poll is refused.
  - Back to idle: after the answer is delivered, nothing is held and no new request is sent.
  - Oversize reply: a reply over 64 KiB is refused without revoking authority.
- **Implementation line ranges:**
  - `hydrate.rs:47–128`
  - `channels.rs:39–129`
  - `auth.rs:158` (`pool`), `308–320` (`channel_pool`, `launch_binding`), `414–600` (bounded post, source status and chunk, `read`)
  - `native/hydration.rs:317–340` (`validate_retained`)

Paths are relative to `crates/aq-worker/src/native/runner/sandbox/` unless they are shown in full.

## Results after main sync

The branch was squashed to `2a85edb` and then merged with `origin/main` at `66cefd8`, giving `f68f8d1`. The following checks were rerun on the merged tree:

| Exact command | Result | Evidence in B6c-evidence |
| --- | --- | --- |
| `cargo test --locked --workspace hydrated_workspace_matches_retained_revision` | 1 passed | `synced-hydrated_workspace_matches_retained_revision.log` |
| `cargo test --locked --workspace idle_sandbox_holds_no_terminal_poll` | 1 passed | `synced-idle_sandbox_holds_no_terminal_poll.log` |
| `cargo test --locked --workspace hydration_refuses_retained_bundle_digest_mismatch` | 1 passed | `synced-hydration_refuses_retained_bundle_digest_mismatch.log` |
| `cargo test --locked -p aq-worker --test sandbox_runner` | 14 passed | `synced-runner.log` |
| `cargo test --locked -p aq-worker --lib native::runner::sandbox` | 10 passed | `synced-sandbox-lib.log` |
| `cargo fmt --check -p aq-worker` | passed | `synced-fmt.log` |
| `cargo clippy --locked -p aq-worker --all-targets -- -D warnings` | passed | `synced-clippy.log` |

The `sandbox_runner` suite was rerun because main changed it, and it exercises the modified `RunnerHttp`. The final documentation commit changes no source.
