# Phase-aware physical inputs (candidate)

An actual accepted-preparation observation and its fsynced first received history are independent of the elapsed original proposal delivery window. This slice extends physical input owners to that explicitly negotiated phase. It does not construct legacy PreparationPlan, initial execution Grant, host permission, renewal, or activation.

## Authority and persisted provenance

Journal format 7 changes input provenance to a strict tagged `InputOrigin`: legacy RecordedPreparation or phase ReceivedPreparation. The phase branch carries the exact accepted request/proposal digest linkage already retained by PhaseAcceptance; validation joins it to that attempt's immutable accepted receipt. Every public phase operation also requires the actual opaque AcceptedPreparation and validates the min(first durable deadline, current actual delivery deadline). Reading JSON or reopening history cannot supply this observation. Material leases come from the accepted lease, never the original offer clock. Artifact/spec/source descriptors remain the original immutable offer pins.

Old journal formats 1–6 refuse before mutation, preserving original bytes. Legacy input APIs explicitly refuse phase attempts. Legacy grant and scoped presence remain legacy-only. Format-7 validation permits physical inputs on exactly one branch; no cross-origin records, hidden conversion, or mixed receipt set.

## API and ownership

InputPipeline adds artifacts_phase, material_phase, hydrate_source_phase, and prepare_inputs_phase taking Arc<AcceptedPreparation>. Private typed context dispatches common physical byte assembly, bounded two-way artifact reads, descriptor-private material installation, and actual Git exchange/staging validation. The shared code never obtains authority by deserializing that context. Journal operations check exact origin before and after awaited work; physical operations keep existing original IDs, digests, pending ranges and unknown holds.

PreparedPhaseInputs retains the actual RenderedStartup, HydratedWorkspace, VerifiedArtifacts and InstalledMaterial owners with a frozen preparation_v2::PrepareRequest. There is no conversion to legacy PreparedInputs. Exact full inline + source manifest/bundle + installed material receipt coverage is validated by frozen 0.1.9. Prepare intent is durable before returning; no phase execution Grant or host call exists in this slice. Caller cancellation does not cancel admitted fsync and never releases durable source/resource holds.

## Bounds and evidence

Existing admitted attempt, aggregate inline byte, request/range, storage and bulk lane bounds remain. Exact operation retries cannot extend the first accepted cap. The positive profile must wait until original proposal expiry before artifact/material/Git operations, yet complete within accepted cap. Tests include old-format byte preservation, branch laundering/rebinding, expired Accepted, changed receipt/deadline, write cuts, cancellation/pending-ID replay, actual private bytes and pinned actual Git, plus final full worker package and strict lint. Synthetic coordinator qualification is distinguished from actual daemon owner interoperability; the root-owned smoke binary/probe supplies the latter separately.

Owned paths: native/journal/{inputs,source,prepared,preparation_v2}.rs and narrow journal.rs; native/preparation.rs, hydration.rs, preparation/{join,phase} child modules; typed internal material installation adapter and focused tests. No binaries, server schema/contracts/routes, host, or provider code.
