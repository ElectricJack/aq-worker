//! Recovery of a host whose guardian was lost (swarm phase 1, ruling B on D2).
//!
//! A guardian proves its own stop: it reaps every descendant and records the kernel's ECHILD.
//! A guardian that was killed with its service (a crash, `systemctl kill`) or with the machine
//! (a reboot) cannot. Its stop is then taken as proven only when every process it could have
//! left is dead by an outside fact:
//!
//! - the machine has rebooted since the guardian started (its recorded boot id differs from
//!   `/proc/sys/kernel/random/boot_id`), or
//! - the service attested at its start ([`attest_prior_invocation_gone`]) that its systemd
//!   unit's cgroup held only this invocation's processes, and the guardian started before this
//!   invocation, on this boot.
//!
//! The proof is recorded in the stopped observation's `stop_reason`. Without one, recovery
//! fails with [`StopUnproven`] and the slot is held for the operator.
use crate::{
    GuardianIdentity, HostObservation, HostPhase, LaunchIntent, SessionHost, StoppedProof,
    read_json, supervisor,
};
use anyhow::Result;
use std::{path::Path, sync::OnceLock, time::Duration};

/// The outside fact that proves a lost guardian's processes are all dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopProof {
    /// The machine rebooted since the guardian started.
    BootChanged,
    /// The service's unit cgroup held only the current invocation when it started, and the
    /// guardian is older than that invocation.
    PriorInvocationGone,
}
impl StopProof {
    pub fn label(self) -> &'static str {
        match self {
            Self::BootChanged => {
                "guardian lost; every host process proven stopped: the machine rebooted since launch (boot id changed)"
            }
            Self::PriorInvocationGone => {
                "guardian lost; every host process proven stopped: the service unit's cgroup held no process of the previous invocation"
            }
        }
    }
}

/// A lost guardian whose stop no outside fact proves: the slot is held for the operator.
#[derive(Debug)]
pub struct StopUnproven(pub String);
impl std::fmt::Display for StopUnproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host stop unproven: {}", self.0)
    }
}
impl std::error::Error for StopUnproven {}

/// (boot id, start ticks of the service process) once the service attested that no process
/// of an earlier invocation remains.
static PRIOR_GONE: OnceLock<(String, u64)> = OnceLock::new();
/// Records that every process started on this boot before `own_start_ticks` (the service
/// process's start) is dead. Only the service calls it, after it checked its unit's cgroup.
pub fn attest_prior_invocation_gone(own_start_ticks: u64) -> Result<()> {
    let (boot, _) = supervisor::machine()?;
    let _ = PRIOR_GONE.set((boot, own_start_ticks));
    Ok(())
}

/// The outside proof that `guardian`'s processes are dead, given this boot's id and the
/// service's attestation.
pub fn stop_proof(
    guardian: &GuardianIdentity,
    boot: &str,
    prior_gone: Option<&(String, u64)>,
) -> Option<StopProof> {
    if guardian.machine_boot_id != boot {
        return Some(StopProof::BootChanged);
    }
    prior_gone
        .filter(|(at, ticks)| at == boot && guardian.process.start_ticks < *ticks)
        .map(|_| StopProof::PriorInvocationGone)
}

/// The stopped proof of `last` (the guardian's last durable observation), made by `proof`.
/// Every field is the guardian's own except: the phase (stopped), the ECHILD flag (all its
/// processes are proven gone) and the stop reason (the proof). A host that had started its
/// pane also gets a reap count of at least the root (which is gone) and, when unobserved, the
/// root status 128 + SIGKILL; a host without a pane keeps none (no root ever ran).
pub fn proven_stop(last: &HostObservation, proof: StopProof) -> StoppedProof {
    let mut observation = last.clone();
    observation.phase = HostPhase::Stopped;
    observation.kernel_wait_echild = true;
    if observation.pane.is_some() {
        observation.reaped_count = observation.reaped_count.max(1);
        observation.root_status = observation.root_status.or(Some(128 + libc::SIGKILL));
    }
    observation.stop_reason = Some(proof.label().into());
    StoppedProof { observation }
}

/// Recovery's stop of a launch: the guardian stops and proves it (`SessionHost::recover`).
/// When that fails and the guardian is lost, the stop is proven by [`stop_proof`] from its last
/// observation: `last` (the journal's), else the launch directory's state. Otherwise the error
/// is [`StopUnproven`]; a live guardian's error is returned as it is (the caller retries).
pub fn recover_stopped(
    root: &Path,
    expected: &LaunchIntent,
    last: Option<&HostObservation>,
) -> Result<StoppedProof> {
    let boot = supervisor::machine()?.0;
    recover_stopped_with(root, expected, last, &boot, PRIOR_GONE.get())
}
/// [`recover_stopped`] with this boot's id and the service's attestation given.
pub fn recover_stopped_with(
    root: &Path,
    expected: &LaunchIntent,
    last: Option<&HostObservation>,
    boot: &str,
    prior_gone: Option<&(String, u64)>,
) -> Result<StoppedProof> {
    let direct = || -> Result<StoppedProof> {
        let mut handle = SessionHost::recover(root, expected)?;
        handle.stop()?;
        handle.wait_stopped(Duration::from_secs(10))?;
        handle.stopped_proof()
    };
    let error = match direct() {
        Ok(proof) => return Ok(proof),
        Err(error) => error,
    };
    let state = || -> Result<HostObservation> {
        let dir = std::fs::canonicalize(root)?.join(expected.launch_id.to_string());
        read_json(&dir.join("state.json"))
    };
    let last = last.cloned().or_else(|| state().ok());
    let Some(last) = last.filter(|o| o.guardian.intent == *expected) else {
        return Err(StopUnproven(format!(
            "no observation of the guardian survives: {error:#}"
        ))
        .into());
    };
    if supervisor::guardian_live(&last.guardian) {
        return Err(error);
    }
    match stop_proof(&last.guardian, boot, prior_gone) {
        Some(proof) => Ok(proven_stop(&last, proof)),
        None => Err(StopUnproven(format!(
            "the guardian was lost and no reboot or empty unit cgroup proves its processes stopped: {error:#}"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessEvidence;
    fn guardian(boot: &str, start_ticks: u64) -> GuardianIdentity {
        let id = uuid::Uuid::from_u128(1);
        GuardianIdentity {
            intent: LaunchIntent {
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
            process: ProcessEvidence {
                pid: 10,
                start_ticks,
            },
            machine_boot_id: boot.into(),
            pid_namespace: "pid:[1]".into(),
        }
    }
    /// The guardian's last durable observation (the journal's Running one).
    pub(crate) fn running(boot: &str, start_ticks: u64) -> HostObservation {
        let process = |pid| ProcessEvidence {
            pid,
            start_ticks: 7,
        };
        HostObservation {
            guardian: guardian(boot, start_ticks),
            phase: HostPhase::Running,
            pane: Some(crate::PaneIdentity {
                server: process(4_000_001),
                pane: process(4_000_002),
                pane_id: "%1".into(),
                session_name: "aq".into(),
            }),
            permit_sequence: 3,
            permit_deadline_boot_ms: 90_000,
            observed_boot_ms: 60_000,
            reaped_count: 0,
            kernel_wait_echild: false,
            root_status: None,
            stop_reason: None,
        }
    }
    fn recover(last: &HostObservation, prior: Option<&(String, u64)>) -> Result<StoppedProof> {
        let root = tempfile::tempdir().unwrap();
        let intent = last.guardian.intent.clone();
        recover_stopped_with(
            &root.path().join("gone"),
            &intent,
            Some(last),
            "boot",
            prior,
        )
    }
    /// Ruling B: a guardian from an earlier boot is proven stopped; so is one of an earlier
    /// invocation when the unit's cgroup held only this invocation (`invocation::holds_only`);
    /// with a foreign process in the cgroup there is no attestation, and the stop is unproven
    /// (the service holds the slot).
    #[test]
    fn recover_proves_a_lost_guardians_stop_or_reports_it_unproven() {
        let earlier_boot = recover(&running("old-boot", 500), None).unwrap();
        let o = earlier_boot.observation();
        assert_eq!(o.phase, HostPhase::Stopped);
        assert!(o.kernel_wait_echild && o.root_status == Some(137));
        assert_eq!(
            o.stop_reason.as_deref(),
            Some(StopProof::BootChanged.label())
        );
        assert_eq!(o.permit_sequence, 3, "the guardian's own observation");
        let attested = ("boot".to_owned(), 900);
        let prior = recover(&running("boot", 500), Some(&attested)).unwrap();
        let reason = prior.observation().stop_reason.clone();
        assert_eq!(
            reason.as_deref(),
            Some(StopProof::PriorInvocationGone.label())
        );
        let held = recover(&running("boot", 500), None).unwrap_err();
        assert!(held.downcast_ref::<StopUnproven>().is_some(), "{held:#}");
        let ours = recover(&running("boot", 950), Some(&attested)).unwrap_err();
        assert!(
            ours.downcast_ref::<StopUnproven>().is_some(),
            "this invocation's guardian"
        );
    }
    #[test]
    fn a_changed_boot_proves_the_stop() {
        let g = guardian("old-boot", 500);
        assert_eq!(
            stop_proof(&g, "new-boot", None),
            Some(StopProof::BootChanged)
        );
    }
    #[test]
    fn the_same_boot_needs_the_attested_empty_cgroup_and_an_older_guardian() {
        let g = guardian("boot", 500);
        assert_eq!(stop_proof(&g, "boot", None), None, "no attestation");
        let attested = ("boot".to_owned(), 900);
        assert_eq!(
            stop_proof(&g, "boot", Some(&attested)),
            Some(StopProof::PriorInvocationGone)
        );
        let ours = guardian("boot", 950);
        assert_eq!(
            stop_proof(&ours, "boot", Some(&attested)),
            None,
            "this invocation's"
        );
        let other_boot = ("earlier".to_owned(), 900);
        assert_eq!(stop_proof(&g, "boot", Some(&other_boot)), None);
    }
}
