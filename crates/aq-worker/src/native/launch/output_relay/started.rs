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
    use crate::native::test_support::profile_history::terminal_attempt as fixture;
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
