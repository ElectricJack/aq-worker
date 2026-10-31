# Portable AQ terminal relay candidate0.1.0

Pure peer envelopes over exact aq-session-protocol0.1.0 identities, controller
leases, literal input commands, output frames and snapshots. It has no daemon,
human-auth API, process, transport or tmux dependency. Types and validation confer
no authority; an adapter must authenticate the node, join exact current sealed
execution authority, and persist commands/receipts under those fences.

`aq.terminal-relay/1` supplies typed fence/ack/revoke, capture, resize receipts and
bounded poll/upload/ack. `aq.terminal-input-epoch/1` separately negotiates monotonic
input epochs and deterministic UUIDv8 IDs. Finite input mode remains distinct;
unknown/retired epochs never imply retry permission. Central peer identity is
independent, with no fake worker identity. The server candidate refuses central
runtime operation until an independent authority adapter exists.

`decode_document` bounds input before deserialization and redacts shape errors.
The archive includes artifacts/peer-schema.json, valid/invalid peer fixtures and
canonical UUID vectors; packaged drift/conformance tests need no daemon/services.
A schema is advisory: typed validation and runtime authorization remain mandatory.
Metadata-only control-upload is the same PeerUpload subset with frames/snapshots/
gaps empty. Worker adapters must prioritize ACKs/receipts on that reserved route.
No transport, renderer, provider, launch, unlimited retention or capacity claim.
