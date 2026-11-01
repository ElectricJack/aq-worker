# Opt-in paired phase terminal producer

This slice uses exact `aq-phase-terminal-protocol`0.1.0, archive SHA256
`1b3e4afbd9356f6d7027b0a4be3fe37e2231b55bdfa10269c87a84bf9568f0bb`.
No published manifest/source or admission019 bytes are rewritten. The worker
workspace alone patches frozen019 for the new package's registry dependency.

The implementation remains test-support-only and uses the existing explicitly
pinned synthetic host. No default driver, provider, renewal, resource release or
production capability is activated. The physical host API and all serialized
host shapes remain unchanged0.1.4.

Private journal format10 adds exactly one Started and one optional Stopped
publication/ACK per host. Formats1–9 refuse unchanged before any historical
reinterpretation, cleanup or new permission. Genuine Running and its complete
phase Started envelope are written atomically in the existing synchronous host
callback. A saved actual opaque `StoppedProof` and its matching envelope likewise
share one safety-only commit. The original accepted/execution receipt digests,
retained Prepare/Accept/Proposal chain, runtime/qualification, server Prepared
checkpoint, exact guardian/pane/kernel and journal ordering are revalidated on
persist and reopen. Serialized facts cannot construct live execution authority.

Before any physical host effect, the outer intent reserves1MiB of logical journal
space for its complete host history, future stop facts, both publications and
ACKs. Existing8MiB snapshot capacity remains unchanged. The private non-outbox
history has a512KiB bound; four public documents have independent64KiB limits,
and their container/counters have an additional1024-byte bound. Their total is
strictly below1MiB. All journal writes and reopen account for the remaining
reservation, so unrelated work cannot consume it. The global revision is charged
at its20-digit ceiling, preserving the reservation across counter growth. This
is logical headroom, not a guarantee against physical ENOSPC or failed fsync.
Native-host admission is consequently at most7 alongside other history and may
be lower; no capacity/performance claim is made for simultaneous host startup.

`InputPipeline::phase_terminal_history(id)` returns historical outbox state.
`report_phase_terminal_started(id)` and `report_phase_terminal_stopped(id)` send
only its fsynced immutable envelope and persist/cache the exact wrapped ACK.
Explicit retries reuse the original operation/digest. Readback or an ACK never
samples a new delivery clock, renews authority or frees a hold. Restart permits
historical report/ACK persistence, not a new host. A canceled HTTP caller or
ambiguous receipt commit leaves a durable queued fact; there are no automatic
mutation retries, redirects, proxy fallback or legacy conversion.

The client sends `aq-phase-terminal-api-version: 1` and the existing protected
node credential to explicit `/v1/worker/profiled-v2/started` and `/stopped` routes.
Replies must be direct matching wrapped receipts with the same version header,
JSON content type and total64KiB limit. Started uses the existing two-slot control
lane. Stopped uses a distinct one-slot client/lane, so blocked Started, bulk and
renewal traffic cannot consume its transport capacity. Physical stop remains
independent of the network and bootstrap/storage lanes.

The narrow first slice has a recovery limitation: if the first Started ACK was
lost and no Started was centrally admitted before expiry, a new historical
Started request may refuse, and Stopped cannot establish a matching central
Started. Both outboxes remain held on refusal. No-Started closure publication is
deferred because the format9 driver intent did not retain the exact checkpoint
digest needed for that proof; no value is invented from a later snapshot. Actual
local stop/Unknown preservation still works. This incomplete closure lane bars a
full lifecycle capability advertisement and dead-worker recovery/product claim.
The daemon's independently qualified paired owner and actual joined process are
separate evidence from synthetic HTTP qualification here.
