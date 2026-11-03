# SBX26 node-shared compiler cache

Task: agile-rapids-34. Approved storage amendment supplied by the project
supervisor in msg-28e4284c4f914e02b24d2b70c4420bbe (rev-steady-rapids).

The node controller loads `sandbox.shared_sccache`, default false, and passes
it to the operator-owned docker-local policy. Enabled launches mount
`aq-<node UUID without hyphens>-sccache` at `/var/cache/sccache`, with
`SCCACHE_DIR=/var/cache/sccache` and `SCCACHE_CACHE_SIZE=10G`. The volume is
shared across the node's attempts and boots, outside attempt work storage,
and excluded from attempt inventory and destruction. Templates supply sccache,
compiler selection and cache permissions. The 10G eviction budget is not a
filesystem quota. This option must never be enabled for untrusted projects:
all attempts on the node can modify the shared cache. The sandbox filesystem
storage budget is 0 GB off and 10 GB on. CARGO_INCREMENTAL=0 was already
implemented by template_tool.py; no additional template build changes are needed.

Backend registration advertises `shared-build-cache` only when enabled. A
request requiring it on a disabled backend is refused before daemon mutation.
aq-admission 0.3.3 adds the closed enum variant, negotiation allowlist entry,
updated schema bounds, and supported/disabled conformance fixtures. Previous
release archives remain unchanged.

The deterministic release archive is
`protocol/releases/aq-admission-0.3.3.tar.gz`, SHA256
`35fb4440141a9985db63a17f3d632dc24d075bb97372d08cdbe192c5879bc2f9`.
Reproduce with `python3 scripts/package-admission.py --check`.
The project supervisor filed aq-platform follow-up **solid-impact-24** for
aq-server's version pin, vendoring, checksum and conformance migration; it
depends on this change landing (msg-e902cd21c493483aa01c3fa4b550857e).

Verification runs use the existing AQ Cargo adapter:

```sh
B5A_COMMAND='<cargo command below>' \
B5A_LOG="$PWD/docs/implementation/shared-sccache-evidence/<name>.log" \
B5A_RESULT="$PWD/docs/implementation/shared-sccache-evidence/<name>.json" \
aq test docs/implementation/B5a-evidence/test_cargo.py -q
```

The adapter enforces CARGO_BUILD_JOBS=2; Rust test threads are limited to 2.
Logs and command/exit receipts live in `shared-sccache-evidence/`.
The initial Docker run compiled the old volume policy while its test expectation
was being updated to the node-scoped name; `docker-local-initial.log` preserves
that failed assertion. The final rebuild passes without weakening the test.

| Check | Result |
| --- | --- |
| `cargo test --locked -p aq-admission@0.3.3 --test sandbox -- --test-threads=2` | 16 passed; protocol.log |
| `cargo test --locked -p aq-worker --test docker_local_backend -- --test-threads=2` | 28 passed, 3 existing live-daemon tests ignored; docker-local.log |
| `cargo test --locked -p aq-worker --lib controller:: -- --test-threads=2` | 56 passed; controller.log |
| `cargo fmt -- --check` | Passed |
| `git diff --check` | Passed |
| `python3 scripts/package-admission.py --check` | Passed; archive hash above |

The Docker evidence uses the existing scripted daemon and validates the actual
Engine API create body and shared-volume lifecycle. It does not claim live
Docker/Podman or real compiler-cache qualification. No new slow, network or
full-system tests were added.
