# Native Prepare input join

This slice joins the format3 input/source owners into a durable Prepare request.
It does not start a host, restore a grant from disk or advertise a runnable worker.
The public admission0.1.7 transport remains unchanged. Journal format4 explicitly
refuses older formats unchanged; migration/restart admission remains separate.

The input is the exact original live Preparation, actual VerifiedArtifact objects
for every original inline descriptor, the opaque HydratedWorkspace and a trusted
installed renderer qualification. Total inline bytes are bounded to8MiB at this
join. Missing, duplicate, changed or merely historical receipts refuse. The join
retains one actual rendered startup payload and the live verified inputs in an
opaque process-local PreparedInputs, so an emitted request is not confused with
payload availability or host permission.

Before recording intent, the bounded storage lane rechecks the current source
staging bytes and actual Git retention/workspace using original durable read
requests, verifies original binding and exact workspace, and reopens all material
installations against independently journaled pins and actual private bytes.
No new source mutation or material retrieval occurs. All original preparation
and source deadlines are checked after these reads and after the durable write.
Cancellation keeps the blocking lane until its actual work completes.

The Prepare request contains exactly the original inline receipts plus the actual
transfer manifest and bundle references/sizes/digests and installed material
receipts. It records the exact prior journal revision/digest, generates one stable
operation ID, and is fsynced before returning. Retry requires fresh actual input
objects and current inspections; it returns the original request without changing
its checkpoint. A failed possibly effective write poisons the writer and retains
all reservations. Reopen exposes history only, never a new PreparedInputs.

The immutable intent has its own canonical digest and bounded association checks
on load. Metadata cannot manufacture a missing verified payload, live source
proof, delivery clock, host permit or installation. No task/slot/source/material
release is introduced. Native polling still needs a separately integrated capacity
preflight matching the configured Git profile; the conservative default21GiB+
1MiB reservation and bounded cold preparation window remain limitations.

Qualification covers exact input sets, material/source changes after hydration,
changed renderer, durable retry identity, four fsync cuts, historical reopen,
and no mutation/host call on refusal, using a synthetic coordinator and real
local journal/material files. The separately pinned actual Git case should also
exercise this join before a full daemon/worker execution claim is made.
