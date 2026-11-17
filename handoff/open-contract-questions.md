# Worker contract dispositions — 2026-11-17

The external handoff directory is read-only in this session, so this copy records the worker's
choices for the server implementer. No wire or vendor source was changed by this worker work.

1. Inventory: the worker shares one allocation high-water and acknowledged sequence per node
   boot. It holds a node gate across publication and offer poll, publication through final
   phase execution, and cancellation polls. A running slot reads the shared latest sequence.
   This avoids requiring the server to accept stale inventory for these operations.
2. Registration: once successfully acknowledged per node invocation; retries preserve one
   operation id, boot and initial header count. Live capacity updates never re-register. No
   undocumented conflict triggers re-registration. A server that loses a registered boot
   must accept replay/history consistently, or require a service restart; arbitrary conflict
   responses are not authority to silently replace a boot.
3. Old server: only v2 400 bad_request negotiates down, cached until restart. Each enrolled
   node runs one queued slot without headers, preserving the old per-attempt boot/register
   path. V1 manual max_slots remains a compatibility setting; v2 ignores it locally and the
   server must ignore it for admission too.
4. Machines retain all enrolled nodes. Lowering nodes explicitly and restarting creates
   recovery-only supervisors for extra credentials. They never register a new boot or poll
   for new work. Every report/release remains under that old node and journal identity.
5. Count zero: registration header is at least 1 (the header forbids 0); live control count 0
   admits no queued or new manual work. Server authority must use its current setting.
6. Superseded boot terminal operations and per-slot offer redelivery/correlation are required
   server behavior. They have not been validated against a running server in this sandbox.
   The requested server spec did not exist when this implementation began.

The server/vendor owner independently added baseline commit fb9f19d after initial reading.
Our delivery commits descend from that commit; their file lists contain no protocol/vendor
changes. The original checkout branch cannot be advanced from this session (shared Git
metadata is read-only), so commits are preserved in target/delivery.git and the delivery
bundle exported under handoff/.
