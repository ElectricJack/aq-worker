use super::*;
use crate::native::journal::launch::{NativeHostHistory, NativeStopped};
/// Closure evidence for a launch that never started: a durable suppression when
/// no host process was ever started, otherwise the host's own stopped proof.
pub(super) fn closure_request(
    a: &Attempt,
    revision: u64,
    state: &crate::native::journal::State,
    operation_id: Uuid,
) -> Result<crate::native::journal::launch::ClosureRequest> {
    let original = original_request(a, revision, state, operation_id)?;
    use crate::native::journal::launch::ClosureRequest;
    if a.refresh_acceptance.is_some() {
        use aq_phase_terminal_protocol::refresh_v1 as wire;
        return crate::native::journal::refresh_terminal::with_context(a, |c| {
            let request = wire::ClosureRequest {
                schema_version: 1,
                negotiation: wire::Negotiation::current()?,
                origin: wire::Origin::for_context(c)?,
                original,
            };
            request.validate_for(c)?;
            Ok(ClosureRequest::Refresh(request))
        });
    }
    crate::native::journal::phase_terminal::with_context(a, |c| {
        let request = aq_phase_terminal_protocol::ClosureRequest {
            schema_version: 1,
            negotiation: aq_phase_terminal_protocol::Negotiation::current()?,
            origin: aq_phase_terminal_protocol::Origin::for_context(c)?,
            original,
        };
        request.validate_for(c)?;
        Ok(ClosureRequest::Frozen(request))
    })
}

fn original_request(
    a: &Attempt,
    revision: u64,
    state: &crate::native::journal::State,
    operation_id: Uuid,
) -> Result<aq_admission::granted_closure::Request> {
    use aq_admission::granted_closure::{Evidence, JournalCheckpoint, Request, SuppressionRecord};
    let h = a.native_host.as_ref().context("closure host absent")?;
    let i = &h.intent;
    let execution = a.phase_scope()?;
    let previous = JournalCheckpoint {
        revision: state.revision,
        digest: crate::native::journal::canonical_digest(state)?,
    };
    let evidence = match (&h.guardian, &h.stopped) {
        (None, _) if !h.start_intended => {
            let record = SuppressionRecord {
                schema_version: 1,
                operation_id,
                suppression_id: Uuid::new_v4(),
                binding: i.binding.clone(),
                grant_id: i.grant_id,
                guardian_id: i.logical_guardian_id,
                launch_id: i.launch_id,
                reservation_id: i.reservation_id,
                original_kernel_boot_id: execution.kernel_boot_id,
                recording_kernel_boot_id: state.kernel_boot_id,
                previous: previous.clone(),
                terminal_revision: revision,
            };
            let record_digest = record.digest()?;
            Evidence::Suppressed {
                record,
                record_digest,
            }
        }
        (Some(_), Some(stopped)) => stopped_evidence(h, stopped, &previous)?,
        _ => anyhow::bail!("unstarted launch has no closure evidence (host not stopped)"),
    };
    Ok(Request {
        schema_version: 1,
        operation_id,
        binding: i.binding.clone(),
        grant_id: i.grant_id,
        guardian_id: i.logical_guardian_id,
        launch_id: i.launch_id,
        reservation_id: i.reservation_id,
        runtime: i.runtime.runtime.clone(),
        qualification: i.runtime.qualification.clone(),
        journal: JournalCheckpoint {
            revision,
            digest: previous.digest,
        },
        evidence,
    })
}
fn stopped_evidence(
    h: &NativeHostHistory,
    stopped: &NativeStopped,
    previous: &aq_admission::granted_closure::JournalCheckpoint,
) -> Result<aq_admission::granted_closure::Evidence> {
    use aq_admission::granted_closure::{DriverIntent, Evidence, HostStoppedProof};
    let i = &h.intent;
    let proof = HostStoppedProof {
        observation: serde_json::from_value(serde_json::to_value(&stopped.observation)?)?,
    };
    let host = h
        .host_intent
        .as_ref()
        .context("closure host intent absent")?;
    Ok(Evidence::HostStopped {
        driver_intent: Box::new(DriverIntent {
            binding: i.binding.clone(),
            grant_id: i.grant_id,
            guardian_id: i.logical_guardian_id,
            reservation_id: i.reservation_id,
            host: serde_json::from_value(serde_json::to_value(host)?)?,
            journal: previous.clone(),
        }),
        proof_digest: proof.digest()?,
        proof,
    })
}
