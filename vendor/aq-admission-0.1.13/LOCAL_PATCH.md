# Local interactive-session patch

This is the next source package for the explicit one-hour interactive policy. Previous vendor directories and archives remain unchanged. No archive/extracted-closure qualification is claimed here. The paired agent-q source supplies the admission policy implementation and focused test; terminal-protocol changes only its dependency/package version, preserving existing wire schemas. Policy2 and the default host permit remain capped at120000ms. Policy3 is selected only for the actual interactive factory and is bounded at3600000ms.


## 0.1.12 (MS1, aq-worker ms1/demo)

Mirror of aq-server `crates/aq-admission` 0.1.12: the long (schema 3) phase
policy also admits the docker-local Claude manifest (Claude 2.1.318,
`aq.claude-result-local-adapter`, terminal input off, container confinement
declared and native not declared). Everything else is byte-identical to 0.1.11.


## 0.1.13 (spec 0015, a worker node runs several agents)

Mirror of aq-server `crates/aq-admission` 0.1.13: adds `src/node_slots.rs` (the
`aq-node-slot` and `aq-node-slots` request headers, their format bound and
parsers) beside the pinned transport documents. No wire document, schema or
fixture changed; everything else is byte-identical to this repository's 0.1.12
mirror. The contract is aq-server `docs/specs/0015-multi-agent-nodes.md`;
`docs/operations/multi-agent-nodes.md` here lists the worker-facing fields.


## 0.1.13, no execution-lease ceiling in the contract

Mirror of aq-server `crates/aq-admission` (branch
`feat/no-admission-execution-ceiling`): `MAX_INTERACTIVE_EXECUTION_MS` is
removed. A schema-3 phase policy accepts any positive
`maximum_execution_lease_ms`, and the long schema declares that field and
`DeliveryClock.valid_for_ms` as integers with `minimum` 1 and no `maximum`. The
lease length is the owner's Setting, carried in the offer; this crate only
refuses zero.

- `src/preparation_shared.rs`: the same three hunks as the server file.
- `src/transport.rs`: `validate_interactive_session` has no upper bound and
  the deadline projection refuses an overflow instead of wrapping. The server
  keeps this code in `src/delivery_clock_interactive.rs` beside its byte-frozen
  `transport.rs`; the behaviour is the same.
- `tests/preparation_refresh.rs`: the policy test follows the server's.

The contract ids are unchanged and the digests of both long contracts change,
because the long schema changed. No earlier digest is accepted:

| Contract id | Version | Digest |
| --- | --- | --- |
| `aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1` | 1 | `ff94a1ee6f91e6ccacaef136654fb49ddbdf713f2f7f6f8d6eff547783554b78` |
| `aq.admission.preparation-v2-contract.interactive-long-v1` | 1 | `11b92feeaef04261dde6fce1a384d7367f127c0b0f76f98b11eb1b170bc23caf` |

Recompute with
`cargo test -p aq-worker --locked --all-features --test native_phase_terminal e0_qual -- --nocapture`.
A node's `negotiation.contract` in `native.json` must carry the first row.
