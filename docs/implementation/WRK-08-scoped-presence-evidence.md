# Initial-grant scoped material presence checkpoint

Source `04e2a32` on the format-6/phase-HTTP union; exact admission 0.1.9 adoption `b54ec5e`. No host, provider, runtime activation, continuing renewal owner, or phase-v2 grant conversion is included.

The producer requires the actual in-process durable initial-grant owner and retained prepared resources, exactly one original admitted material, and an acknowledged node/boot inventory. A permanent per-pipeline singleton owns bounded monotonic publication state. Inventory starts from the driver's acknowledged sequence and asserts no globally installed credentials. Positive reports require fresh descriptor-relative private-file reads and matching original canonical installation record, kernel boot, byte digest and length, with authority checked before and after I/O. Missing/Unknown remain distinct scoped history. Canceled reads and sends retain their operation; retries never recapture or retime it.

Qualification uses actual private filesystem operations and loopback HTTP with synthetic coordinator/runtime/adapter qualification. The 17 focused cases cover same-byte file/record/directory replacement during read, absent/symlink/hardlink/FIFO/mode changes, positive expiry during read, late negative observation, the two-second read bound, canceled read/send, lost ACK, immutable retry, actual normalized inventory acknowledgement, singleton ownership, cross-pipeline binding, malformed receipts, and control progress with both bulk slots blocked. These tests do not qualify the daemon's separate 029 owner or a real credential provider.

Validation used `CARGO_TARGET_DIR=/tmp/aq-worker-presence-target`, incremental disabled, build jobs 2, debug information disabled, and `RUST_TEST_THREADS=2`:

- `cargo test -p aq-worker --all-features --locked --offline`: **196 passed, 5 existing ignored**. This run precedes only an equivalent nested-if to let-chain lint rewrite.
- Exact final source focused `--lib presence_scenarios`: **17 passed**, 10.89s.
- Exact final source opt-in `--lib pinned_git_ -- --ignored`: **2 passed**, 4.41s, using pinned actual aq-git SHA256 `c2ff26f5e63008aec0fbd4082a547e87825e651d4aba0744c4b9fdf752d58c39`.
- Strict all-target/all-feature worker Clippy and workspace formatting passed. Earlier compilation preflights and the single collapsible-if lint failure are retained, rather than treated as successful qualification.

Retained logs and source inventory are in `evidence/wrk08-scoped-presence-04e2a32/`. Admission archives remain exact: 0.1.9 SHA256 `30f4cfca11ff6840eab325026dfea33dd55c23e25099dedb303b52a1ede75b49`; prior 0.1.7 SHA256 `46283490c98c5b082aa8dbfd4ccde230db3d97828fcbd71ff5b0526837bae545`.

Bounds are one current inventory publication and at most one current operation per admitted attempt (at most 64), finite wire bodies and receipts, and one original material per grant. Sequence ownership is process-local; reopen is historical only and cannot construct a positive producer. No automatic retries, background refresh, inventory-derived execution authority, secret bytes in report/logs, or local resource release is introduced. The existing client may discard a late structurally valid Grant before producing the opaque live observation; that earlier unknown/held edge remains outside this producer.
