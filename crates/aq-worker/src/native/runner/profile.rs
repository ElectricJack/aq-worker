//! Exact profile data for retained histories; never execution authority.
use super::*;
use crate::native::journal::Attempt;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalProfile {
    Frozen,
    Refresh,
}
impl TerminalProfile {
    pub(super) fn for_execution(attempt: &Attempt) -> Result<Self> {
        let frozen = attempt.phase_acceptance.is_some()
            && attempt.phase_preparation.is_some()
            && attempt.phase_prepared.is_some()
            && attempt.phase_execution.is_some();
        let no_frozen = attempt.phase_acceptance.is_none()
            && attempt.phase_preparation.is_none()
            && attempt.phase_prepared.is_none()
            && attempt.phase_execution.is_none();
        let refresh = attempt.refresh_acceptance.is_some()
            && attempt.refresh_preparation.is_some()
            && attempt.refresh_prepared.is_some()
            && attempt.refresh_execution.is_some();
        let no_refresh = attempt.refresh_acceptance.is_none()
            && attempt.refresh_preparation.is_none()
            && attempt.refresh_prepared.is_none()
            && attempt.refresh_execution.is_none();
        attempt.phase_scope()?;
        match (frozen, no_frozen, refresh, no_refresh) {
            (true, false, false, true) => Ok(Self::Frozen),
            (false, true, true, false) => Ok(Self::Refresh),
            _ => anyhow::bail!("runner missing or mixed retained preparation profile"),
        }
    }
    pub(super) fn for_attempt(attempt: &Attempt) -> Result<Self> {
        let selected = Self::for_execution(attempt)?;
        let host = attempt
            .native_host
            .as_ref()
            .context("runner native host history absent")?;
        let running = host
            .running
            .as_ref()
            .context("runner genuine Running absent")?;
        match (selected, &host.terminal, &host.refresh_terminal) {
            (Self::Frozen, Some(t), None) => {
                ensure!(
                    t.started.request.original.digest()? == running.request.digest()?
                        && t.started.request.original.binding.session
                            == host.intent.terminal.session,
                    "runner retained Running/session differs"
                );
                crate::native::journal::phase_terminal::with_context(attempt, |context| {
                    t.started.request.validate_for(context)?;
                    Ok(())
                })?;
            }
            (Self::Refresh, None, Some(t)) => {
                ensure!(
                    t.started.request.original.digest()? == running.request.digest()?
                        && t.started.request.original.binding.session
                            == host.intent.terminal.session,
                    "runner retained Running/session differs"
                );
                crate::native::journal::refresh_terminal::with_context(attempt, |context| {
                    t.started.request.validate_for(context)?;
                    Ok(())
                })?;
            }
            _ => anyhow::bail!("runner terminal profile differs from retained execution"),
        }
        Ok(selected)
    }
    pub(super) async fn report_started(self, pipeline: &InputPipeline, id: Uuid) -> Result<()> {
        match self {
            Self::Frozen => {
                pipeline.report_phase_terminal_started(id).await?;
            }
            Self::Refresh => {
                pipeline.report_refresh_terminal_started(id).await?;
            }
        }
        Ok(())
    }
    pub(super) async fn report_stopped(self, pipeline: &InputPipeline, id: Uuid) -> Result<()> {
        match self {
            Self::Frozen => {
                pipeline.report_phase_terminal_stopped(id).await?;
            }
            Self::Refresh => {
                pipeline.report_refresh_terminal_stopped(id).await?;
            }
        }
        Ok(())
    }
    pub(super) fn complete(self, attempt: &Attempt) -> Result<bool> {
        ensure!(
            Self::for_attempt(attempt)? == self,
            "runner final profile differs from configured profile"
        );
        let host = attempt.native_host.as_ref().unwrap();
        match self {
            Self::Frozen => {
                let t = host.terminal.as_ref().unwrap();
                if let Some(ack) = &t.started.receipt {
                    ack.validate_for(&t.started.request)?;
                } else {
                    return Ok(false);
                };
                let Some(stopped) = &t.stopped else {
                    return Ok(false);
                };
                crate::native::journal::phase_terminal::with_context(attempt, |context| {
                    stopped.request.validate_for(context, &t.started.request)?;
                    Ok(())
                })?;
                if let Some(ack) = &stopped.receipt {
                    ack.validate_for(&stopped.request)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::Refresh => {
                let t = host.refresh_terminal.as_ref().unwrap();
                if let Some(ack) = &t.started.receipt {
                    ack.validate_for(&t.started.request)?;
                } else {
                    return Ok(false);
                };
                let Some(stopped) = &t.stopped else {
                    return Ok(false);
                };
                crate::native::journal::refresh_terminal::with_context(attempt, |context| {
                    stopped.request.validate_for(context, &t.started.request)?;
                    Ok(())
                })?;
                if let Some(ack) = &stopped.receipt {
                    ack.validate_for(&stopped.request)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
}
