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


## 0.1.13 (W1, Codex in the docker-local sandbox)

Mirror of aq-server `crates/aq-admission` 0.1.13 after W1s: adds
`src/docker_local.rs` (`DOCKER_LOCAL_PROFILES`, the exact docker-local
executors with their pinned recipe digests), and the long (schema 3) phase
policy in `src/preparation_shared.rs` admits each row of that table instead of
Claude alone. Both files are byte-identical to the server's; `src/lib.rs`
gains only the module and its export. The server's `never_started` and
`delivery_clock_interactive` modules are not mirrored: the worker does not use
them.
