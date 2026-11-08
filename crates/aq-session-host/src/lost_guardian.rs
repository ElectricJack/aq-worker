//! Recovery of a host whose guardian was lost (swarm phase 1, D2).
//!
//! A guardian proves its own stop: it reaps every descendant and records the kernel's ECHILD.
//! A guardian killed with its service (a crash, `systemctl kill`) or with the machine (a
//! reboot) cannot. An outside fact can still prove every process it could have left dead:
//!
//! - the machine rebooted since the guardian started (its recorded boot id differs from
//!   `/proc/sys/kernel/random/boot_id`): `StopProof::BootChanged`;
//! - the service attested at its start ([`attest_prior_invocation_gone`]) that its systemd
//!   unit's cgroup held only this invocation's processes, and the guardian started before this
//!   service process on this boot (kernel start times, `/proc/<pid>/stat` field 22):
//!   `StopProof::CgroupTeardown`.
//!
//! The stopped contract (aq-admission `stopped`, frozen) carries only the guardian's own
//! evidence, and no value the guardian never observed may be put in it (supervisor ruling).
//! Until a stop contract with an explicit proof kind exists, a lost guardian is therefore
//! never reported stopped: recovery fails with [`StopUnproven`], which names the outside proof
//! that is available, and the service holds the slot for the operator.
use crate::{GuardianIdentity, HostObservation, LaunchIntent, SessionHost, StoppedProof};
use crate::{read_json, supervisor};
use anyhow::Result;
use aq_protocol::StopProof;
use std::{path::Path, sync::OnceLock, time::Duration};

/// A lost guardian: its stop is not reported. `proof` is the outside proof that its processes
/// are dead, when one holds; the stopped contract cannot carry it yet.
#[derive(Debug)]
pub struct StopUnproven {
    pub proof: Option<StopProof>,
    pub detail: String,
}
impl std::fmt::Display for StopUnproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.proof {
            Some(p) => write!(
                f,
                "host stop not reported: the guardian was lost; an outside proof holds ({}: {}), but the stopped contract has no kind for it yet: {}",
                p.kind(),
                serde_json::to_string(p).unwrap_or_default(),
                self.detail
            ),
            None => write!(
                f,
                "host stop unproven: the guardian was lost and no reboot or unit teardown proves its processes stopped: {}",
                self.detail
            ),
        }
    }
}
impl std::error::Error for StopUnproven {}

/// What the service attested at its start: every process started on `boot` before
/// `start_ticks` (the service process's start) is dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorGone {
    pub boot: String,
    pub start_ticks: u64,
    pub invocation_id: String,
    pub unit: String,
}
static PRIOR_GONE: OnceLock<PriorGone> = OnceLock::new();
/// Records the service's attestation. Only the service calls it, once, after it checked that
/// its unit's cgroup holds only itself and its descendants.
pub fn attest_prior_invocation_gone(
    own_start_ticks: u64,
    invocation_id: String,
    unit: String,
) -> Result<()> {
    let (boot, _) = supervisor::machine()?;
    let _ = PRIOR_GONE.set(PriorGone {
        boot,
        start_ticks: own_start_ticks,
        invocation_id,
        unit,
    });
    Ok(())
}

/// The outside proof that `guardian`'s processes are dead, from real facts only: this boot's
/// id, and the service's attestation. The guardian's age is its kernel start time as recorded
/// in its identity (never a file time).
pub fn stop_proof(
    guardian: &GuardianIdentity,
    boot: &str,
    prior_gone: Option<&PriorGone>,
) -> Option<StopProof> {
    if guardian.machine_boot_id != boot {
        let old_boot_id = uuid::Uuid::parse_str(&guardian.machine_boot_id).ok()?;
        let new_boot_id = uuid::Uuid::parse_str(boot).ok()?;
        return Some(StopProof::BootChanged {
            old_boot_id,
            new_boot_id,
        });
    }
    let p =
        prior_gone.filter(|p| p.boot == boot && guardian.process.start_ticks < p.start_ticks)?;
    Some(StopProof::CgroupTeardown {
        invocation_id: p.invocation_id.clone(),
        unit: p.unit.clone(),
    })
}

/// Recovery's stop of a launch: the guardian stops and proves it (`SessionHost::recover`).
/// When that fails and the guardian is not live (lost), the error is [`StopUnproven`], naming
/// the outside proof [`stop_proof`] finds from the guardian's last observation (`last`, the
/// journal's, else the launch directory's state). A live guardian's error is returned as it is
/// (the caller retries).
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
    prior_gone: Option<&PriorGone>,
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
        let detail = format!("no observation of the guardian survives: {error:#}");
        return Err(StopUnproven {
            proof: None,
            detail,
        }
        .into());
    };
    if supervisor::guardian_live(&last.guardian) {
        return Err(error);
    }
    Err(StopUnproven {
        proof: stop_proof(&last.guardian, boot, prior_gone),
        detail: format!("{error:#}"),
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostPhase, ProcessEvidence};
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
    const OLD: &str = "00000000-0000-4000-8000-00000000000a";
    const NOW: &str = "00000000-0000-4000-8000-00000000000b";
    fn attested() -> PriorGone {
        PriorGone {
            boot: NOW.into(),
            start_ticks: 900,
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: "aq-node@t.service".into(),
        }
    }
    fn unproven(last: &HostObservation, prior: Option<&PriorGone>) -> StopUnproven {
        let root = tempfile::tempdir().unwrap();
        let intent = last.guardian.intent.clone();
        let gone = root.path().join("gone");
        let e = recover_stopped_with(&gone, &intent, Some(last), NOW, prior).unwrap_err();
        e.downcast::<StopUnproven>().unwrap()
    }
    /// The supervisor's ruling on the proof format: a lost guardian is never reported
    /// stopped with values it did not observe. The outside proof is found from real facts (a
    /// changed boot id; the attested teardown and the guardian's kernel start time) and named,
    /// and the stop stays unreported until a stop contract with a proof kind exists.
    #[test]
    fn a_lost_guardian_is_never_reported_stopped_and_its_outside_proof_is_named() {
        let rebooted = unproven(&running(OLD, 500), None);
        assert_eq!(
            rebooted.proof,
            Some(StopProof::BootChanged {
                old_boot_id: OLD.parse().unwrap(),
                new_boot_id: NOW.parse().unwrap(),
            })
        );
        assert!(rebooted.to_string().contains("boot_changed"), "{rebooted}");
        let torn = unproven(&running(NOW, 500), Some(&attested()));
        assert_eq!(
            torn.proof.as_ref().map(StopProof::kind),
            Some("cgroup_teardown")
        );
        assert!(
            unproven(&running(NOW, 500), None).proof.is_none(),
            "no attestation"
        );
        let ours = unproven(&running(NOW, 950), Some(&attested()));
        assert!(
            ours.proof.is_none(),
            "a guardian of this invocation is not covered"
        );
        assert_eq!(running(NOW, 500).phase, HostPhase::Running);
    }
    #[test]
    fn a_proof_needs_a_changed_boot_or_the_attested_teardown_of_the_same_boot() {
        let g = guardian(NOW, 500);
        assert_eq!(stop_proof(&g, NOW, None), None);
        let mut other = attested();
        other.boot = OLD.into();
        assert_eq!(
            stop_proof(&g, NOW, Some(&other)),
            None,
            "attested on another boot"
        );
        let proof = stop_proof(&g, NOW, Some(&attested())).unwrap();
        assert_eq!(proof.validate(NOW), Ok(()));
        let rebooted = stop_proof(&guardian(OLD, 500), NOW, None).unwrap();
        assert_eq!(rebooted.validate(OLD), Ok(()));
    }
}
