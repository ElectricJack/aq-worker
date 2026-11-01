# Durable accepted-preparation journal checkpoint

Source `3326b59` plus redelivery follow-up `08eb15c` adds journal format6.
One atomic fsynced write selects the phase protocol and stores the exact original
reservation, immutable proposal and Accept request. Legacy Accept/plan paths refuse
that choice. All original resource holds remain charged through expiry, failures
and restart. Formats1–5 are refused unchanged before pending-file cleanup.

Actual opaque HTTP observations are required to record a received accepted lease.
The first transmission's actual response and boot-clock samples survive later
replays, including replay after proposal expiry before the first local receipt
write. Received history remains immutable; a separate current-owner check combines
it with the still-live opaque observation and the most conservative clock cap.
Expiry before/during persistence leaves history and holds, never usable authority.
No input, execution or host permission is reconstructed from disk.

Poll redelivery may carry a later delivery clock for the same immutable publication.
The owner requires identical absolute expiry and every non-clock field, and an
original publication time no later than that delivery. Both actual records are
retained separately; neither clock is rewritten and the existing local cap is not
resampled. A changed expiry, earlier delivery or changed offer content refuses.

The [exact qualification](WRK-08-phase-journal-qualification.json) records179 passing
all-feature worker tests, five explicit ignores, two actual-Git legacy regressions,
strict all-target/all-feature lint and formatting. Eight journal tests and seven
HTTP tests are subsets. Fsync cuts cover intent and receipt publication; checksum-
valid association corruption, expiry, protocol mixing and restart refusal are checked.
Independent review found no remaining authority blocker in this bounded scope.

A lost/aborted Accept reply with no opaque observation remains unknown and held.
After original proposal expiry this slice cannot retry it as a first send; future
qualified recovery must resolve it. Current checks do not adopt phase inputs,
issue execution grants, launch a host/provider, settle/release resources or activate
the native driver. Actual daemon phase-journal qualification is a separate next step.
