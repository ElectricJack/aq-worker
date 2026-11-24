use super::*;

pub(super) fn running(boot: &str) -> aq_session_host::HostObservation {
    let id = Uuid::from_u128(1);
    let process = |pid, start_ticks| aq_session_host::ProcessEvidence { pid, start_ticks };
    aq_session_host::HostObservation {
        guardian: aq_session_host::GuardianIdentity {
            intent: aq_session_host::LaunchIntent {
                launch_id: id,
                session: aq_session_protocol::SessionIdentity::Remote {
                    node_id: id,
                    boot_id: id,
                    attempt_id: id,
                    assignment_generation: 1,
                    session_id: id,
                },
                manifest_digest: aq_session_protocol::Digest::of(b"m"),
                configuration_digest: aq_session_protocol::Digest::of(b"c"),
            },
            nonce: id,
            process: process(4_000_000, 500),
            machine_boot_id: boot.into(),
            pid_namespace: "pid:[4026531836]".into(),
        },
        phase: aq_session_host::HostPhase::Running,
        pane: None,
        permit_sequence: 3,
        permit_deadline_boot_ms: 90_000,
        observed_boot_ms: 60_000,
        reaped_count: 0,
        kernel_wait_echild: false,
        root_status: None,
        stop_reason: None,
    }
}
