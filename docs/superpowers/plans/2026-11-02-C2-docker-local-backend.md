# C2 docker-local Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `DockerLocalBackend` that implements C1's `PlacementBackend` against a dedicated rootless sandbox daemon, under a fixed launch policy that offers cannot change.

**Architecture:** The backend talks Docker Engine API v1.44 over a configured Unix socket, through a `DockerTransport` trait. Production uses a small hyper http1 client. Tests use a scripted in-memory double that can hang, drop replies and lie. Each call runs under a bounded timeout. Any timeout, transport error or unexpected status means "unknown", never "gone", and is never followed by a delete. Identity lives in container and volume labels plus a deterministic name. Stop and destroy are proven by re-inspecting.

**Tech Stack:** Rust 2024. New direct deps: hyper 1 (client, http1), hyper-util 0.1 (tokio), http-body-util 0.1 and bytes 1. All are already in `Cargo.lock`. Daemon: rootless Podman 5.8.8, static build, through its Docker-compatible API.

**Spec:** AQ task calm-delta-18.2 (C2). Read with aq-server `docs/architecture/0005-sandboxed-execution-and-deployment-topology.md` §4, §5.1, §8.3, §13 and §14 (decisions 11 and 14), and with C1 (`docs/implementation/C1.md`, `crates/aq-worker/src/backend/`).

## Global Constraints

- `cargo … --locked` against the committed lockfile. Commit lockfile changes along with the manifest.
- Refuse the host's main daemon (SBX10, §5.1). Use only the configured dedicated socket.
- Fixed launch policy:
  - allowlisted template digests only;
  - `Privileged: false`, `CapDrop: ["ALL"]`, no `CapAdd`, no devices, `no-new-privileges`;
  - no host binds except the per-attempt volume and declared mount aliases;
  - CPU, memory and PID limits from the reservation;
  - `NetworkMode: "bridge"`, no published ports.
- Deliver material through archive upload only, never in env, argv, labels or logs.
- Advertise none of `vm-isolation`, `nested-docker` or `suspend-snapshot`. Unsupported required capabilities refuse explicitly.
- When the daemon is unreachable, paused or timing out, inventory is unknown, never gone, and no destroy is issued.
- Don't claim hostile-code isolation for shared-kernel containers. Confinement is `container`-only (§14.14). The probe evidence goes in C2.md.
- Don't edit `aq-server/docs/architecture/*` or `docs/specs/*`. Proposed changes go in evidence plus a `docs` task.
- Run tests through `aq test` with the C1 wrapper (`docs/implementation/C1-evidence/test_cargo_check.py`, `-n 0 -s`), with `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`.
- Out of scope: controller reconcile, adoption and sweeps (B5b); the allowlist file format (C3b); images (C3a, C3c); Codex flags (C6a); autostart (E6a, E6b); nested Docker.

## Review Focus

1. A socket path that symlinks to `/var/run/docker.sock`, or any root-owned or non-rootless daemon, must be refused at connect. Pinned in Task 2.
2. A container with the deterministic name but a different request hash, node or boot label must give `Conflict` on create. It must never be adopted, overwritten or deleted. Pinned in Task 3.
3. If the daemon answers 204 to DELETE but the container or volume still inspects as present, destroy must be `Unconfirmed`. Pinned in Task 4.
4. A runner that already exited before `stop` (Docker 304) must still give inspect-confirmed proof. A never-started container must be stopped without starting it. Pinned in Task 4.
5. An oversized archive in a fixture or handoff read must fail closed at the cap and not be buffered without bound. Pinned in Task 4.

---

### Task 1: Contract extensions and test plumbing

**Files:**
- Modify: `crates/aq-worker/src/backend/types.rs` (`CreateRequest`, new `ResourceLimits`, `ConfinementMode`)
- Modify: `crates/aq-worker/src/backend/mod.rs` (`pub mod docker_local;`, conformance gate)
- Modify: `crates/aq-worker/Cargo.toml` and `Cargo.lock`
- Create: `crates/aq-worker/src/backend/docker_local/mod.rs` (module skeleton)
- Test: in-module tests in `types.rs`

**Interfaces:**
- Produces `ResourceLimits { cpu_millis: u32, memory_bytes: u64, pids: u32 }` (Serialize, Deserialize, Clone, Copy, Eq, Debug).
- Produces new `CreateRequest` fields, both `#[serde(default)]`:
  - `resources: Option<ResourceLimits>`;
  - `mount_aliases: Vec<String>`.
- Produces builders `CreateRequest::with_resources(self, ResourceLimits) -> Self` and `with_mount_aliases(self, Vec<String>) -> Self`. `CreateRequest::new` keeps its signature (`None`, empty).
- Produces `ConfinementMode { Native, Container }` (kebab-case serde).
- Produces feature `backend-conformance = []`. `test-support` includes it, and `conformance` is gated on it.
- Produces a self dev-dependency `aq-worker = { path = ".", features = ["backend-conformance"] }`, so that plain `cargo test` integration tests reach `backend::conformance` without enabling all of `test-support`. Verified in a scratch crate.

- [ ] **Step 1: Write failing tests:**
  - `create_request_without_resources_deserializes`: C1-shaped JSON without the new keys parses, with `resources == None` and empty `mount_aliases`.
  - `resource_limits_reject_zero`: `validate()` is `Err(InvalidRequest)` when `cpu_millis == 0`, when `pids == 0`, or when `memory_bytes < 6 * 1024 * 1024`.
  - `mount_alias_names_are_validated`: rejects `""`, `"../x"` and `"a/b"`; accepts `"repo-cache"`. Pattern: `[a-z0-9][a-z0-9-]{0,62}`.
- [ ] **Step 2: Run** `cargo test --locked -p aq-worker --lib backend::types`. Expected: compile FAIL, because the fields don't exist yet.
- [ ] **Step 3: Implement** the fields, the builders and the `validate()` extensions. Add the feature, the self dev-dep and the four deps:
  - `hyper = { version = "1", features = ["client", "http1"] }`;
  - `hyper-util = { version = "0.1", features = ["tokio"] }`;
  - `http-body-util = "0.1"`;
  - `bytes = "1"`.
  Then refresh the lock with `cargo metadata` (not `--locked`) and check that only aq-worker's dependency list changed.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --locked -p aq-worker --test backend_conformance`. Expected: PASS. The C1 count is unchanged (17).
- [ ] **Step 5: Commit** `feat(worker): add reservation limits and mount aliases to CreateRequest`.

### Task 2: Config, transport and main-daemon refusal

**Files:**
- Create: `crates/aq-worker/src/backend/docker_local/config.rs`
- Create: `crates/aq-worker/src/backend/docker_local/transport.rs`
- Modify: `crates/aq-worker/src/backend/docker_local/mod.rs`
- Create: `crates/aq-worker/tests/docker_local_backend.rs`
- Create: `crates/aq-worker/tests/support/docker_double.rs`, included with `#[path] mod`

**Interfaces:**
- `DockerLocalConfig` (Deserialize, Clone) has these fields:
  - `socket: PathBuf`;
  - `templates: Vec<AllowedTemplate { image: String, digest: TemplateDigest }>`. `image` must end in `@{digest}`;
  - `mount_aliases: BTreeMap<String, MountAlias { host_path: PathBuf, target: String, read_only: bool }>`;
  - `runner_command: Option<Vec<String>>`;
  - `material_path: String`, default `/aq/private/material`;
  - `material_uid: u32` and `material_gid: u32`, default 0;
  - `api_timeout_ms: u64`, default 10000;
  - `stop_grace_secs: u32`, default 10;
  - `max_read_bytes: u64`, default 1048576.
- `DockerLocalConfig::validate(&self) -> Result<(), BackendError>` refuses with `InvalidRequest("refusing the host's main Docker daemon")` when:
  - the canonical or literal socket path is `/var/run/docker.sock` or `/run/docker.sock`;
  - the socket's owner uid is not `geteuid()`.
  It also refuses an alias target outside `/aq/` or a non-absolute host path.
- `DockerTransport: Send + Sync + 'static` has `fn send(&self, DockerRequest) -> impl Future<Output = Result<DockerResponse, TransportError>> + Send`.
  - `DockerRequest { method: DockerMethod{Get,Post,Put,Delete}, path: String, content_type: Option<&'static str>, body: Zeroizing<Vec<u8>> }`. `path` includes `/v1.44` and the query.
  - `DockerResponse { status: u16, body: Vec<u8> }`.
  - `TransportError(String)` never holds request bodies.
- `UnixSocketTransport::new(PathBuf)`: one connection per request, `hyper::client::conn::http1::handshake` over `TokioIo<UnixStream>`, `Host: docker`.
- `DockerLocalBackend<T: DockerTransport = UnixSocketTransport>::connect(config, transport) -> Result<Self, BackendError>`:
  - runs `validate()`;
  - sends `GET /v1.44/info` and requires `SecurityOptions` to contain `"name=rootless"`, else `InvalidRequest("sandbox daemon must be rootless")`;
  - maps a timeout to `Uncertain(Timeout)`.
- `DockerLocalBackend::connect_unix(config)`.
- `DockerLocalBackend::CONFINEMENT: &[ConfinementMode] = &[ConfinementMode::Container]`, and `require_confinement(ConfinementMode) -> Result<(), BackendError>`, which gives `UnsupportedCapability` for `Native`.
- Test double `ScriptedDocker` (`Clone`, with shared `Arc<Mutex<State>>`):
  - in-memory containers and volumes;
  - `requests() -> Vec<(DockerMethod, String)>`;
  - `pause()` and `resume()`: while paused, every `send` hangs until resume;
  - `script(Rule { method, path_prefix, effect: FailBefore(u16) | DropReplyAfterApply | LieOkWithoutApply })` for one-shot faults;
  - `rootless: bool`.

- [ ] **Step 1: Write failing tests** in `docker_local_backend.rs`:
  - `refuses_main_docker_socket`: `validate()` errors for `/var/run/docker.sock`, and for a tempdir symlink to it when that exists.
  - `refuses_socket_not_owned_by_current_user`: a socket path whose owner is root, `/var/run/docker.sock` when present, otherwise skipped with an explicit assertion message.
  - `refuses_non_rootless_daemon`: `connect` against `ScriptedDocker { rootless: false }` gives `Err(InvalidRequest("sandbox daemon must be rootless"))`.
  - `connect_times_out_as_uncertain`: a paused double and `api_timeout_ms: 100` give `Err(Uncertain(Timeout))`.
  - `confinement_is_container_only`: `require_confinement(Native)` gives `Err(UnsupportedCapability)`, and `Container` gives `Ok`.
  Tests that use a double point `socket` at a tempdir Unix socket the test binds, so the owner check passes.
- [ ] **Step 2: Run** `cargo test --locked -p aq-worker --test docker_local_backend`. Expected: compile FAIL.
- [ ] **Step 3: Implement** `config.rs`, `transport.rs` and `connect`. Wrap every `send` in `tokio::time::timeout(api_timeout)`.
- [ ] **Step 4: Run** the Step 2 command. Expected: 5 PASS.
- [ ] **Step 5: Commit** `feat(worker): docker-local config, transport and main-daemon refusal`.

### Task 3: Launch policy, create, inspect, inventory

**Files:**
- Create: `crates/aq-worker/src/backend/docker_local/policy.rs` (labels, create body, label parsing)
- Modify: `crates/aq-worker/src/backend/docker_local/mod.rs` (`PlacementBackend` impl: `kind`, `capabilities`, `create`, `inspect`, `list_owned`)
- Test: `crates/aq-worker/tests/docker_local_backend.rs`, plus double routes in `support/docker_double.rs`

**Interfaces:**
- Labels on the container and the volume:
  - `aq.managed-by=aq-worker`, `aq.backend=docker-local`;
  - `aq.node`, `aq.boot`, `aq.attempt` as hyphenated UUIDs;
  - `aq.template-digest`, `aq.hard-deadline-ms`;
  - `aq.request-sha256`: hex sha256 of `serde_json::to_vec(request)`.
- Names: the container is `request.name`; the volume is `{name}-work`. `backend_id` is the container name.
- `policy::create_body(config, request, image) -> serde_json::Value`. Pinned values:
  - `Env: []`, `ExposedPorts: {}`, `Cmd` from `runner_command` (omitted when `None`), `WorkingDir: "/aq/work"`.
  - In `HostConfig`:
    - `Privileged: false`, `CapAdd: []`, `CapDrop: ["ALL"]`, `Devices: []`, `DeviceCgroupRules: []`, `SecurityOpt: ["no-new-privileges"]`;
    - `NanoCpus: cpu_millis * 1_000_000`, `Memory` and `MemorySwap` both set to `memory_bytes`, `PidsLimit: pids`;
    - `NetworkMode: "bridge"`, `PortBindings: {}`, `PublishAllPorts: false`;
    - `Binds: []`, `IpcMode: "private"`, `RestartPolicy: {"Name": "no"}`, `AutoRemove: false`;
    - `Mounts`: first `{"Type":"volume","Source":"{name}-work","Target":"/aq/work","ReadOnly":false}`, then a bind for each requested alias, in request order.
- `policy::entry_from_labels(labels, state, running_processes, disk_present) -> Result<InventoryEntry, BackendError>`. Missing or malformed labels give `Uncertain(UnknownState)`.
- State mapping:
  - `created` → Created with `Some(0)`;
  - `running` → Running with `None`;
  - `exited` or `dead` with `Running == false` → Stopped with `Some(0)`;
  - anything else, including `paused`, → Unknown with `None`.
  - A labelled volume with no container → Stopped with `Some(0)` and `disk_present: true`.
- `capabilities()` returns `vec![PersistentDiskDuringAttempt]`.
- `create` does these steps in order:
  1. `validate()`.
  2. Refuse missing resources: `InvalidRequest("docker-local requires reservation limits")`.
  3. Refuse an unallowlisted digest: `InvalidRequest("template digest not allowlisted")`.
  4. Refuse an undeclared alias: `InvalidRequest("undeclared mount alias")`.
  5. Refuse unsupported capabilities with `UnsupportedCapability`.
  6. Inspect by name. If it exists with the same request hash and owner, return its identity; otherwise `Conflict`.
  7. Create the volume, or accept an existing one with identical labels, otherwise `Conflict`.
  8. `POST /containers/create?name=`.
- `list_owned(node)` uses `GET /containers/json?all=true` and `GET /volumes`, both with label filters `aq.managed-by=aq-worker` and `aq.node={node}`. Either call failing gives `Err`.
- `pull_template(&mut self, digest) -> Result<(), BackendError>` (inherent): `POST /images/create?fromImage={image}`, then `GET /images/{image}/json` must return 200.

- [ ] **Step 1: Write failing tests** against `ScriptedDocker`:
  - `launch_policy_is_fixed`: assert each pinned field above on the recorded create body, plus `Labels` and `?name=aq-…`.
  - `refuses_unallowlisted_template`, `refuses_missing_resources`, `refuses_undeclared_mount_alias`.
  - `refuses_unsupported_capabilities`: one case each for VmIsolation, NestedDocker and SuspendSnapshot.
  - `advertises_no_vm_nested_or_snapshot`.
  - `create_is_idempotent_for_identical_request`: two creates give the same identity and exactly one `POST /containers/create`.
  - `create_conflicts_on_foreign_same_name`: seed a container with the same name and a different `aq.boot` or request hash. Expect `Err(Conflict)`, no DELETE, and no create recorded.
  - `lost_create_reply_is_discoverable`: `DropReplyAfterApply` on create; `list_owned` finds it.
  - `list_owned_scopes_node_and_includes_old_boots`.
  - `inventory_failure_is_err`: a 500 on `/volumes` gives `Err`, not `Ok(vec![])`.
- [ ] **Step 2: Run** `cargo test --locked -p aq-worker --test docker_local_backend`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement** `policy.rs` and the create, inspect and list paths. Extend the double's routes to cover what the backend sends.
- [ ] **Step 4: Run** the Step 2 command. Expected: all PASS.
- [ ] **Step 5: Commit** `feat(worker): docker-local fixed launch policy and labelled inventory`.

### Task 4: Material, runner, stop, destroy and scripted conformance

**Files:**
- Create: `crates/aq-worker/src/backend/docker_local/tar.rs`
- Modify: `crates/aq-worker/src/backend/docker_local/mod.rs` (`deliver_private_material`, `start_runner`, `stop`, `destroy`, `read_file`)
- Test: `crates/aq-worker/tests/docker_local_backend.rs`

**Interfaces:**
- `tar::material_archive(path: &str, data: &[u8], uid: u32, gid: u32) -> Zeroizing<Vec<u8>>`. It writes ustar directory entries with mode 0700 and the file with mode 0600.
- `tar::single_file(archive: &[u8], max: u64) -> Result<Vec<u8>, BackendError>`.
- `deliver_private_material` only works while the container is `created`; otherwise `InvalidState`. It sends `PUT /containers/{name}/archive?path=/` with `application/x-tar`.
- `start_runner` sends `POST /containers/{name}/start`. 204 and 304 are OK.
- `read_file(&mut self, sandbox, path) -> Result<Vec<u8>, BackendError>` (inherent) uses `GET /containers/{name}/archive?path=`. With no container it returns `NotFound`; reading a volume-only sandbox needs a helper container and is out of scope. Bodies above `max_read_bytes` give `InvalidRequest("archive exceeds read cap")`.
- `stop`:
  1. Inspect and verify labels.
  2. If `created`, the container was never started: `DELETE /containers/{name}` (no force), confirm 404 and confirm the volume is present.
  3. Otherwise send `POST /stop?t={grace}` (204 or 304), using a timeout of `api_timeout + grace`.
  4. Re-inspect. Only `Stopped`, `Some(0)` gives `Confirmed(StoppedWriterProof::stopped)`.
  Any error or unconfirmed step gives `Unconfirmed(Timeout | BackendFailure | StillRunning | UnknownState)`.
- `destroy`:
  1. `authorizes()`, else `Unconfirmed(Unauthorized)`.
  2. `stop` must be Confirmed.
  3. `DELETE` the container (404 OK) and the volume.
  4. Re-inspect both. Only two 404s give `Confirmed(destroyed)`.

- [ ] **Step 1: Write failing tests:**
  - `material_travels_only_in_archive_upload`: the material bytes appear in no request path and in no non-PUT body; the PUT archive contains them at `aq/private/material` with mode 0600.
  - `material_refused_after_start`: `InvalidState`.
  - `stop_after_runner_exited_is_confirmed`: the double returns 304 on stop and `exited` on inspect. Expect Confirmed.
  - `stop_never_started_keeps_volume`: expect Confirmed. Inspect then gives a Stopped entry with `disk_present`, and no `/start` was recorded.
  - `stop_unconfirmed_when_still_running`: the double ignores stop. Expect `Unconfirmed(StillRunning)`.
  - `destroy_requires_authorization`: an authorization for another sandbox gives `Unconfirmed(Unauthorized)` with zero DELETEs.
  - `destroy_with_lying_delete_is_unconfirmed`: `LieOkWithoutApply` on the volume DELETE gives Unconfirmed, and the volume is still listed.
  - `read_file_refuses_oversized_archive`: cap 16 bytes against a 1 KiB archive.
  - `scripted_conformance`: `conformance::run_all` with a `ScriptedFixture`. On `/start`, the double's runner writes `fixture-complete\n` to `/aq/work/fixture-output` if the material file exists, then sets the container to `exited`. `inject` maps `CrashAfterCreate` to `DropReplyAfterApply` on `POST /containers/create` and `DestroyFailure` to `FailBefore(500)` on `DELETE /containers/`.
- [ ] **Step 2: Run** `cargo test --locked -p aq-worker --test docker_local_backend`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement** `tar.rs` and the four operations.
- [ ] **Step 4: Run** the Step 2 command. Expected: all PASS.
- [ ] **Step 5: Commit** `feat(worker): docker-local material delivery and inspect-proven stop/destroy`.

### Task 5: Suspend/resume, live qualification and daemon tooling

**Files:**
- Modify: `crates/aq-worker/tests/docker_local_backend.rs`
- Create: `crates/aq-worker/tests/support/fault_transport.rs`. This is `FaultTransport<T>`, which wraps any transport with the same `Rule` effects plus request recording, so live tests share the fault semantics.
- Create: `scripts/sandbox-daemon/aq-sandbox-podman.sh` (already drafted) and `scripts/sandbox-daemon/aq-sandbox-podman.service` (a systemd user unit)

**Interfaces:**
- Live env:
  - `AQ_DOCKER_LOCAL_SOCKET` (required);
  - `AQ_DOCKER_LOCAL_IMAGE`, default `docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec`;
  - `AQ_DOCKER_LOCAL_DAEMON_PID` (needed for the live suspend test).
  A missing required env makes an ignored test **panic** with the variable's name. It never passes silently.
- Live runner command: `["sh","-c","test -s /aq/private/material && printf 'fixture-complete\\n' > /aq/work/fixture-output"]`.

- [ ] **Step 1: Write the tests:**
  - `wsl_suspend_resume_reconciles_sandboxes` (not ignored, scripted double):
    1. create, deliver, start (the double keeps the container `running`) and `journal.record_created`;
    2. `pause()`;
    3. `list_owned` gives `Err(Uncertain(Timeout))`, `inspect` gives `Err`, and `destroy(operator_released)` gives `Unconfirmed(Timeout)`;
    4. assert that no DELETE was recorded during the pause;
    5. `resume()`;
    6. `list_owned` contains the sandbox with `disk_present`, and the volume is listed;
    7. `reconcile(Some(&journal), Some(entry), Unretained, Active)` is `Adopt` or `Preserve`;
    8. clean up with stop and release.
  - `live_conformance` (`#[ignore]`): `run_all` with a `LiveFixture` on a fresh random node UUID. It calls `pull_template` first. `observe_fixture` and `verify_and_retain` use `read_file("/aq/work/fixture-output")`. Cleanup is best-effort and scoped to the fixture's node label.
  - `live_launch_policy_applied` (`#[ignore]`): the runner prints `/proc/self/status` CapEff and NoNewPrivs, plus cgroup `cpu.max`, `memory.max` and `pids.max`, into `/aq/work/policy`. With limits 500 m CPU, 64 MiB and 32 PIDs, assert `CapEff: 0000000000000000`, `NoNewPrivs: 1`, `50000 100000`, `67108864` and `32`.
  - `live_wsl_suspend_resume_reconciles_sandboxes` (`#[ignore]`): the same as the scripted test, except that pausing is `kill(pid, SIGSTOP)` on the daemon with `api_timeout_ms: 1000`, resuming is `SIGCONT`, and the container keeps running.
- [ ] **Step 2: Run** `cargo test --locked --workspace wsl_suspend_resume_reconciles_sandboxes`. Expected: 1 passed. Then start the daemon (`AQ_SANDBOX_SINGLE_UID=1 scripts/sandbox-daemon/aq-sandbox-podman.sh serve`) and run `cargo test --locked -p aq-worker --test docker_local_backend -- --ignored` with the env set. Expected: 3 passed, 0 filtered. Fix and re-run until green.
- [ ] **Step 3: Write** the user unit:
  - `ExecStart=%h/…/aq-sandbox-podman.sh serve`;
  - `Environment=AQ_SANDBOX_SINGLE_UID=1` (commented, as the fallback);
  - `Delegate=cpu memory pids`.
- [ ] **Step 4: Commit** `test(worker): docker-local suspend/resume and live qualification`.

### Task 6: Definition of done and evidence

**Files:**
- Create: `docs/implementation/C2.md` and `docs/implementation/C2-evidence/*.log`

- [ ] **Step 1: Run** `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, t1 and the tests deliverable, through `aq test`. Compare the results against the known baseline failures (bold-harbor-86, grand-crest-46, calm-forge-98) and save the logs.
- [ ] **Step 2: Write** C2.md. It covers:
  - the Podman-vs-Docker choice and why;
  - the confinement decision (`container`) with the `unshare -U` probe output;
  - the single-UID limit and the pending uidmap request;
  - the WSL2 install notes: rootless setup, the socket path `$XDG_RUNTIME_DIR/aq-sandbox/podman.sock`, cgroup v2 delegation via `user@.service` (cpu, memory, pids), and autostart caveats (a systemd user unit needs `systemd=true` in `wsl.conf` and lingering; there is no autostart before the first login unless `loginctl enable-linger`);
  - the evidence table;
  - proposed design-doc changes.
- [ ] **Step 3: Commit, squash** to one implementation commit, merge `origin/dev` (or `origin/main`), re-run t1, `aq git push`, then close.
