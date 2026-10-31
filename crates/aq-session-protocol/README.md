# AQ session protocol v1

`aq-session-protocol` 0.1.0 is a standalone pure contract crate. It owns profiled
launch and terminal envelopes; it imports no worker runtime or older protocol
crate. Its `artifacts/` directory is included in Cargo packages. The repository
publishes identical copies in `protocol/session-v1/`.

Use `decode::<ContractDocument>` for bounded untrusted JSON and `Validate` again
at admission/effect boundaries. Serde rejects unknown fields at every record and
mutation variant. `contract_schema()` publishes structural bounds; normative
cross-field, UTF-8 byte, authority and digest rules also require Rust validation.
`Text` is nonempty, control-free and at most 512 UTF-8 bytes. Collections and wire
batches have explicit caps. All time, byte, token and cost units are integers.
No generic settings object, executable command, filesystem path or secret-value
field exists. Credentials are immutable profile references only.

Run `cargo test -p aq-session-protocol` and
`cargo run -p aq-session-protocol --example export` from the repository root.
Exporter changes must accompany reviewed fixture/schema changes; drift tests
compare the generator with packaged fixtures and schema. No service, provider,
network, sibling checkout or tmux executable is needed.

This crate establishes contract conformance, not transport implementation,
authority authentication, process ownership, durable journaling, renderer or tmux
qualification. WRK-10/14 and AQ-17/21 must establish those runtime properties.
