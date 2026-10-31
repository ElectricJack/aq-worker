# Local worker implementation evidence

Current guardian candidate: [worker-recovery.md](worker-recovery.md).
Prior M2 corrected candidate: [worker-candidate.md](worker-candidate.md).
The retained test log records 51 passing component checks.

The foundation source was copied from Agent Q revision
`eed842362f29016ee508ca200f3bb3da2473dc08`. The connector files
`backoff.rs`, `client.rs`, `config.rs`, and `main.rs` retain their behavior;
the extraction retains `$XDG_CONFIG_HOME/agent-q/worker.toml`, strict field
parsing, node identity, mode 0600, no redirects, Cloudflare service headers,
and retry/revocation behavior. `crates/aq-protocol` is now the canonical
foundation package; the daemon consumes its pinned immutable source artifact.
No protocol v1 execution fields were introduced.

The worker owns the `aq-worker-protocol` protocol-2 candidate (initial package
0.1.0, corrected candidate 0.1.1), with generated
schema, validators, fixture peer (`test-support` feature), and executable
conformance tests. Generated schema/fixture copies inside the crate are package
inputs; source-workspace CI checks them against the canonical protocol artifacts.
No daemon/graph/database/runtime implementation dependencies enter that package.

Recorded validation on Linux x86_64 with Rust 1.96.1, 2026-10-30:

- `cargo test --workspace --locked`: initial extraction passes (20 tests), no
  PostgreSQL, daemon, provider credentials or sibling build dependency.
- `cargo test --locked -p aq-worker-protocol --features test-support`: 7 checks
  pass including schema drift, lost acceptance replay/decreasing TTL, ID conflict,
  expiry/fencing, immutable observation/report replay, cancellation race and
  retained stale results. Further worker lifecycle evidence is recorded separately.

This revision does not claim hosted/image qualification, supported real harnesses,
Git/source retention, graph completion, process-tree containment, full restart
reattachment, or product gates. These remain explicit later acceptance boundaries.
