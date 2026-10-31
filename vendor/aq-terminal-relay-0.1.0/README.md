# AQ terminal relay core candidate0.1.0

Pure bounded canonical controller/input/output contracts, sharing published
session protocol/API0.1.0 unchanged. Plans expose no effects until a synchronous
or asynchronous journal acknowledges an actual atomic CAS of ledger+outbox.
Unknown delivery survives restart/takeover and requires a fresh current-controller
capture; same-ID changed bytes, stale identities and retired epochs refuse.

Production adapters must use Ledger::new_profiled and Replay::new_profiled with
the exact verified sealed TerminalPolicy. Protocol-max constructors are contract
conveniences, not permission defaults. Smaller input/pending/frame/replay ceilings
and output-only mode remain binding. Replayable effects recheck current scope and
execution/lease; Input/EpochInput are never replayable outbox effects.

The archive includes deterministic public schema/fixtures; tests model pure peers,
failed commits and lost acknowledgements. A pure value mutation is not durable
SQL, an input receipt is not proof of process ownership, and opaque snapshots do
not qualify a renderer. The separately qualified server adapter owns current
authentication, authority/ACL joins, encryption, node-outbound transport and quotas.
Generic operation IDs remain finite1024; epoch input has a bounded64-receipt window
and durable retired prefix. No unlimited attach or500-agent envelope is claimed.
