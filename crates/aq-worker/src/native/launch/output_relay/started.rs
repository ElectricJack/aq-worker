//! Retained nominal lifecycle facts for the one existing output relay.
//! These are data, never a way to construct a live host or an output owner.
use super::*;
use crate::native::journal::{Attempt, launch::NativeHostHistory};

pub(super) enum RetainedStarted {
    Frozen(aq_phase_terminal_protocol::StartedRequest),
    Refresh(aq_phase_terminal_protocol::refresh_v1::StartedRequest),
}
impl RetainedStarted {
    fn from_history(history: &NativeHostHistory) -> Result<Self> {
        let retained = match (&history.terminal, &history.refresh_terminal) {
            (Some(t), None) => {
                t.started.request.validate()?;
                Self::Frozen(t.started.request.clone())
            }
            (None, Some(t)) => {
                t.started.request.validate()?;
                Self::Refresh(t.started.request.clone())
            }
            _ => anyhow::bail!("native output missing or mixed terminal profile"),
        };
        let running = history
            .running
            .as_ref()
            .context("native output genuine Running absent")?;
        ensure!(
            retained.original().digest()? == running.request.digest()?
                && retained.original().binding.session == history.intent.terminal.session,
            "native output Started differs from retained Running/session"
        );
        Ok(retained)
    }
    pub(super) fn capture(attempt: &Attempt) -> Result<Self> {
        let history = attempt
            .native_host
            .as_ref()
            .context("native output host history absent")?;
        let retained = Self::from_history(history)?;
        match &retained {
            Self::Frozen(request) => {
                ensure!(
                    attempt.refresh_acceptance.is_none() && attempt.refresh_execution.is_none(),
                    "native output frozen profile differs from execution"
                );
                crate::native::journal::phase_terminal::with_context(attempt, |context| {
                    request.validate_for(context)?;
                    Ok(())
                })?;
            }
            Self::Refresh(request) => {
                ensure!(
                    attempt.phase_acceptance.is_none() && attempt.phase_execution.is_none(),
                    "native output refresh profile differs from execution"
                );
                crate::native::journal::refresh_terminal::with_context(attempt, |context| {
                    request.validate_for(context)?;
                    Ok(())
                })?;
            }
        }
        Ok(retained)
    }
    fn original(&self) -> &aq_admission::started::Request {
        match self {
            Self::Frozen(request) => &request.original,
            Self::Refresh(request) => &request.original,
        }
    }
    fn matches(&self, other: &Self) -> Result<bool> {
        match (self, other) {
            (Self::Frozen(a), Self::Frozen(b)) => Ok(a.digest()? == b.digest()?),
            (Self::Refresh(a), Self::Refresh(b)) => Ok(a.digest()? == b.digest()?),
            _ => Ok(false),
        }
    }
    /// The request and receipt stay in the same nominal validator. Only the
    /// already validated common original receipt is borrowed for relay Register.
    pub(super) fn acknowledgment<'a>(
        &self,
        history: &'a NativeHostHistory,
    ) -> Result<&'a aq_admission::started::Receipt> {
        let current = Self::from_history(history)?;
        ensure!(
            self.matches(&current)?,
            "native output retained Started changed profile or bytes"
        );
        match self {
            Self::Frozen(request) => {
                let ack = history
                    .terminal
                    .as_ref()
                    .unwrap()
                    .started
                    .receipt
                    .as_ref()
                    .context("native output Started ACK pending")?;
                ack.validate_for(request)?;
                Ok(&ack.original)
            }
            Self::Refresh(request) => {
                let ack = history
                    .refresh_terminal
                    .as_ref()
                    .unwrap()
                    .started
                    .receipt
                    .as_ref()
                    .context("native output Started ACK pending")?;
                ack.validate_for(request)?;
                Ok(&ack.original)
            }
        }
    }
    pub(super) fn ensure_same(&self, current: &Self) -> Result<()> {
        ensure!(
            self.matches(current)?,
            "native output retained Started changed profile or bytes"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    /// Published synthetic protocol data only: never passed to Journal/live host
    /// constructors. The shared helper cannot construct a relay or live permit.
    fn fixture(refresh: bool, acknowledged: bool) -> Attempt {
        let (terminal, phase): (Value, Value) = if refresh {
            (serde_json::from_slice(include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.1/artifacts/refresh-v1/fixtures.json")).unwrap(),
             serde_json::from_slice(include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-refresh-v1.fixture.json")).unwrap())
        } else {
            (serde_json::from_slice(include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json")).unwrap(),
             serde_json::from_slice(include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-v2.fixture.json")).unwrap())
        };
        let request = &terminal["started_request"];
        let original = &request["original"];
        let digest = Digest::of(b"synthetic history metadata, no live owner");
        let runtime =
            json!({"runtime":original["runtime"],"qualification":original["qualification"]});
        let intent = json!({"execution_digest":digest,"binding":original["binding"],"grant_id":original["grant_id"],"logical_guardian_id":original["guardian_id"],"launch_id":original["launch_id"],"reservation_id":original["reservation_id"],"runtime":runtime,"terminal":{"session":original["binding"]["session"],"terminal_id":Uuid::from_u128(123),"epoch":1},"host_root":"/synthetic-only","root_device":1,"root_inode":1,"pid_namespace":"pid:[1]","profile_digest":digest,"executable":original["runtime"],"helper":original["runtime"],"tmux":original["runtime"],"renderer":original["runtime"],"columns":80,"rows":24,"deadline_ns":20,"hard_deadline_ns":30,"created_ns":1,"created_revision":1,"started_operation_id":original["operation_id"],"stopped_operation_id":Uuid::from_u128(124)});
        let publication = json!({"started":{"request":request,"recorded_revision":2,"receipt":if acknowledged {terminal["started_receipt"].clone()}else{Value::Null}},"stopped":null});
        let mut host = json!({"intent":intent,"host_intent":null,"guardian":null,"start_intended":true,"baseline":null,"running":{"observation":original["observation"],"request":original,"recorded_revision":2},"started_receipt":null,"stopped":null,"uncertain":null,"terminal":null,"refresh_terminal":null});
        host[if refresh {
            "refresh_terminal"
        } else {
            "terminal"
        }] = publication;
        let reservation = json!({"offer":phase["proposal"]["original"],"runtime":runtime,"reserved_at_ns":1,"deadline_ns":2,"disk_bytes":1,"reservation_id":original["reservation_id"],"reservation_operation_id":Uuid::from_u128(125),"accept_operation_id":phase["accept"]["original"]["operation_id"],"guardian_id":original["guardian_id"],"launch_id":original["launch_id"]});
        let received = json!({"delivery":{"schema_version":2,"receipt":phase["accepted"],"delivery":{"nonce":Uuid::from_u128(126),"clock":aq_admission::transport::DeliveryClock::new(1500,3000).unwrap()}},"receipt_digest":digest,"sent_ns":1,"received_ns":2,"deadline_ns":3,"observed_before_write_ns":2,"record_revision":2});
        let acceptance = json!({"proposal":phase["proposal"],"proposal_digest":digest,"request":phase["accept"],"request_digest":digest,"created_ns":1,"original_deadline_ns":2});
        let prepared = json!({"request":phase["prepare"],"request_digest":digest,"prepared_at_ns":2,"origin":{"protocol":if refresh{"refresh"}else{"phase"},"record":received}});
        let execution = json!({"delivery":phase["delivery"],"request_digest":digest,"accepted_digest":digest,"runtime":runtime,"kernel_boot_id":Uuid::from_u128(127),"clock_source":"linux_clock_boottime","sent_ns":1,"received_ns":2,"first_send_deadline_ns":3,"deadline_ns":4,"hard_deadline_ns":5,"observed_before_write_ns":2,"record_revision":3});
        let mut attempt = json!({"reservation":reservation,"acceptance":null,"acceptance_digest":null,"preparation":null,"downloads":{},"material_downloads":{},"source":null,"prepared":null,"initial_grant":null,"phase_acceptance":null,"phase_preparation":null,"phase_prepared":null,"phase_execution":null,"refresh_acceptance":null,"refresh_preparation":null,"refresh_prepared":null,"refresh_execution":null,"native_host":host});
        let prefix = if refresh { "refresh" } else { "phase" };
        for (suffix, value) in [
            ("acceptance", acceptance),
            ("preparation", received),
            ("prepared", prepared),
            ("execution", execution),
        ] {
            attempt[format!("{prefix}_{suffix}")] = value;
        }
        serde_json::from_value(attempt).unwrap()
    }
    #[test]
    fn frozen_and_refresh_capture_validate_their_exact_context_and_ack() {
        for refresh in [false, true] {
            let attempt = fixture(refresh, true);
            let retained = RetainedStarted::capture(&attempt).unwrap();
            assert_eq!(matches!(retained, RetainedStarted::Refresh(_)), refresh);
            let acknowledgment = retained
                .acknowledgment(attempt.native_host.as_ref().unwrap())
                .unwrap();
            assert_eq!(
                acknowledgment.operation_id,
                retained.original().operation_id
            );
            assert!(acknowledgment.authority_revision_at_record > 0);
        }
    }
    #[test]
    fn pending_ack_allows_capture_but_refuses_registration_for_both_profiles() {
        for refresh in [false, true] {
            let attempt = fixture(refresh, false);
            let retained = RetainedStarted::capture(&attempt).unwrap();
            assert!(
                retained
                    .acknowledgment(attempt.native_host.as_ref().unwrap())
                    .is_err()
            );
            retained
                .acknowledgment(fixture(refresh, true).native_host.as_ref().unwrap())
                .unwrap();
        }
    }
    #[test]
    fn mixed_profile_wrong_context_and_cross_profile_replacements_are_refused() {
        let old = fixture(false, true);
        let new = fixture(true, true);
        let old_retained = RetainedStarted::capture(&old).unwrap();
        let new_retained = RetainedStarted::capture(&new).unwrap();
        assert!(old_retained.ensure_same(&new_retained).is_err());
        assert!(
            old_retained
                .acknowledgment(new.native_host.as_ref().unwrap())
                .is_err()
        );
        assert!(
            new_retained
                .acknowledgment(old.native_host.as_ref().unwrap())
                .is_err()
        );
        let mut mixed = old.clone();
        mixed.native_host.as_mut().unwrap().refresh_terminal =
            new.native_host.as_ref().unwrap().refresh_terminal.clone();
        assert!(RetainedStarted::capture(&mixed).is_err());
        let mut wrong = new.clone();
        wrong.refresh_execution = None;
        assert!(RetainedStarted::capture(&wrong).is_err());
        let mut wrong = new.clone();
        wrong
            .refresh_acceptance
            .as_mut()
            .unwrap()
            .proposal
            .policy
            .maximum_observation_ms += 1;
        assert!(RetainedStarted::capture(&wrong).is_err());
    }
    #[test]
    fn changed_running_or_ack_cannot_replace_retained_request() {
        for refresh in [false, true] {
            let mut attempt = fixture(refresh, true);
            let retained = RetainedStarted::capture(&attempt).unwrap();
            let host = attempt.native_host.as_mut().unwrap();
            host.running.as_mut().unwrap().request.operation_id = Uuid::new_v4();
            assert!(retained.acknowledgment(host).is_err());
            let mut attempt = fixture(refresh, true);
            let host = attempt.native_host.as_mut().unwrap();
            if refresh {
                host.refresh_terminal
                    .as_mut()
                    .unwrap()
                    .started
                    .receipt
                    .as_mut()
                    .unwrap()
                    .request_digest = Digest::of(b"changed");
            } else {
                host.terminal
                    .as_mut()
                    .unwrap()
                    .started
                    .receipt
                    .as_mut()
                    .unwrap()
                    .request_digest = Digest::of(b"changed");
            }
            assert!(retained.acknowledgment(host).is_err());
        }
    }
}
