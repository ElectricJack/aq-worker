//! Fresh local sandbox execution. This path never registers, polls or accepts.
pub mod agent_access;
pub mod auth;
mod completion;
pub mod confinement;
mod execution;
pub mod gemini_proxy;
pub mod harness_config;
mod journal;
pub mod mail;
mod manifest;
pub mod native_probe;
pub mod private;
pub mod proxy;
mod renewal;
pub mod responses;
pub mod subscription_login;
pub use manifest::FrozenLaunch;
pub(crate) fn boot_ns() -> anyhow::Result<u64> {
    super::super::http::boot_ns()
}
pub mod channels;
pub mod disk_watch;
pub mod output;
mod output_runtime;
mod startup;
pub use startup::SandboxStartup;
pub mod permit_transport;
mod transport;
use crate::config::WorkerConfig;
use anyhow::{Context, Result, ensure};
use aq_admission_sandbox::sandbox::DelegatedPermit;
use aq_session_host::LocalEnvironment;
use aq_session_protocol::Digest;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use subscription_login::{HarnessExit, LoginFailure, LoginKind, LoginService, SubscriptionLogin};
use tokio::sync::watch;
pub use transport::{RenewalDelivery, RenewalOutcome, RunnerTransport, TransportFuture};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptKey {
    pub launch_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CentralReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
    pub artifact_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetentionReceipts {
    pub source: Option<CentralReceipt>,
    pub result: Option<CentralReceipt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishedReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
}
/// Constructed only from an exact guardian stopped proof, never deserialized.
pub struct StoppedAttempt {
    key: AttemptKey,
    workspace: PathBuf,
    evidence: Vec<u8>,
}
impl StoppedAttempt {
    pub fn key(&self) -> &AttemptKey {
        &self.key
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub fn evidence(&self) -> &[u8] {
        &self.evidence
    }
}
/// Constructed only after both checked receipts are durable locally.
pub struct RetainedAttempt {
    stopped: StoppedAttempt,
    receipts: RetentionReceipts,
}
impl RetainedAttempt {
    pub fn stopped(&self) -> &StoppedAttempt {
        &self.stopped
    }
    pub fn receipts(&self) -> &RetentionReceipts {
        &self.receipts
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Finished,
    Preserved {
        reason: &'static str,
        stop_proven: bool,
    },
    /// The harness exited on a subscription-login authentication failure. The
    /// login was already written back and wiped; the disk is preserved.
    LoginFailed {
        failure: LoginFailure,
        stop_proven: bool,
    },
}
/// C6b subscription login for one sandbox attempt (calm-delta-18.10.1).
pub struct SandboxLogin<'a, S: LoginService> {
    /// `subscription_login::RunnerLogins` in production.
    pub service: &'a mut S,
    pub kind: LoginKind,
    /// Codex only: a private home that must not exist yet. It is created as
    /// the agent user in separated-user mode, so it must be outside runner
    /// state there. Claude logins use the spawn environment and no files.
    pub codex_home: Option<PathBuf>,
    /// Runner-only state that keeps a refreshed login whose write-back failed.
    pub files: &'a private::RunnerFiles,
    /// The adapter's authentication-failure signal for an exited harness. A
    /// sandbox attempt launches once, so `AuthenticationFailed` is final:
    /// `credential_invalid`, not retryable (a newer login is a new attempt).
    pub classify: fn(&aq_session_host::HostObservation) -> HarnessExit,
}
/// No login: the uninhabited service type for the login-free entrypoints.
enum NoLogin {}
impl LoginService for NoLogin {
    async fn fetch_newest(&mut self, _: LoginKind) -> Result<subscription_login::IssuedLogin> {
        match *self {}
    }
    async fn write_back(
        &mut self,
        _: Uuid,
        _: i64,
        _: &[u8],
    ) -> Result<subscription_login::WriteBack> {
        match *self {}
    }
}
type ActiveLogin<'a, S> = Option<(SandboxLogin<'a, S>, SubscriptionLogin)>;
/// Write back and wipe the login before any disk is preserved or retained.
async fn finish_login<S: LoginService>(
    active: &mut ActiveLogin<'_, S>,
    journal: &Path,
    deadline: u64,
) -> Result<()> {
    let Some((login, started)) = active.take() else {
        return Ok(());
    };
    let remaining = deadline.saturating_sub(boot_ns().unwrap_or(deadline));
    let until = tokio::time::Instant::now() + Duration::from_nanos(remaining);
    started
        .finish(login.service, journal, login.files, until)
        .await
        .map(drop)
}
pub struct RunOptions {
    /// Default 15 seconds. Smaller intervals are useful for deterministic fixtures.
    pub renewal_interval: Duration,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            renewal_interval: Duration::from_secs(15),
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn run<'a>(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: impl Into<SandboxStartup<'a>>,
    transport: &mut impl RunnerTransport,
    shutdown: watch::Receiver<bool>,
    options: RunOptions,
) -> Result<RunOutcome> {
    execution::run(
        execution::Site {
            worker,
            frozen,
            state_dir,
            local,
        },
        permit,
        execution::Driver {
            transport,
            shutdown,
            options,
        },
        execution::Harness {
            startup: startup.into(),
            agent: None,
            login: None::<SandboxLogin<'_, NoLogin>>,
        },
    )
    .await
}
/// Separated-user entrypoint. Bootstrap the runner before starting Tokio, keep
/// state private, and hydrate an agent-accessible workspace outside that state.
#[allow(clippy::too_many_arguments)]
pub async fn run_as_agent<'a>(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: impl Into<SandboxStartup<'a>>,
    transport: &mut impl RunnerTransport,
    shutdown: watch::Receiver<bool>,
    options: RunOptions,
    agent: aq_session_host::users::AgentIdentity,
) -> Result<RunOutcome> {
    check_agent_state(state_dir, local)?;
    execution::run(
        execution::Site {
            worker,
            frozen,
            state_dir,
            local,
        },
        permit,
        execution::Driver {
            transport,
            shutdown,
            options,
        },
        execution::Harness {
            startup: startup.into(),
            agent: Some(agent),
            login: None::<SandboxLogin<'_, NoLogin>>,
        },
    )
    .await
}
fn check_agent_state(state_dir: &Path, local: &LocalEnvironment) -> Result<()> {
    private::check_directory(state_dir)?;
    ensure!(
        !local.workspace.starts_with(state_dir),
        "agent workspace must be outside runner state"
    );
    Ok(())
}
/// `run` (agent `None`) or `run_as_agent` with a subscription login installed
/// before the harness spawn and written back and wiped on every teardown path
/// (completion, cancel, expiry, failure, stop-and-preserve) before any disk is
/// preserved. The receipt is journaled in runner-only state.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_login<'a, S: LoginService>(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: impl Into<SandboxStartup<'a>>,
    transport: &mut impl RunnerTransport,
    shutdown: watch::Receiver<bool>,
    options: RunOptions,
    agent: Option<aq_session_host::users::AgentIdentity>,
    login: SandboxLogin<'_, S>,
) -> Result<RunOutcome> {
    if agent.is_some() {
        check_agent_state(state_dir, local)?;
        ensure!(
            login
                .codex_home
                .as_deref()
                .is_none_or(|home| home.is_absolute() && !home.starts_with(state_dir)),
            "agent login home must be outside runner state"
        );
    }
    execution::run(
        execution::Site {
            worker,
            frozen,
            state_dir,
            local,
        },
        permit,
        execution::Driver {
            transport,
            shutdown,
            options,
        },
        execution::Harness {
            startup: startup.into(),
            agent,
            login: Some(login),
        },
    )
    .await
}
fn local_permit(
    frozen: &FrozenLaunch,
    sequence: u64,
    now: u64,
    deadline: u64,
) -> Result<aq_session_host::ExecutionPermit> {
    let permit = aq_session_host::ExecutionPermit {
        session: frozen.manifest.bindings.session.clone(),
        manifest_digest: aq_session_protocol::canonical_digest(&frozen.manifest)?,
        sequence,
        issued_boot_ms: now / 1_000_000,
        expires_boot_ms: deadline / 1_000_000,
    };
    permit.validate_at(&permit.session, &permit.manifest_digest, now / 1_000_000)?;
    Ok(permit)
}
fn stop_reason(
    handle: &aq_session_host::SessionHandle,
    now: u64,
    deadline: u64,
) -> Option<&'static str> {
    if now >= deadline {
        return Some("renewal deadline expired");
    }
    match handle.observe() {
        Ok(observation) => match observation.phase {
            aq_session_host::HostPhase::Stopping | aq_session_host::HostPhase::Stopped => Some(
                if observation.stop_reason.as_deref() == Some("harness pane exited") {
                    "harness exited"
                } else {
                    "guardian stopped"
                },
            ),
            aq_session_host::HostPhase::StopUncertain => Some("stop uncertain"),
            _ => None,
        },
        Err(_) => Some("host observation unavailable"),
    }
}
fn validate_receipts(stopped: &StoppedAttempt, receipts: &RetentionReceipts) -> Result<()> {
    let source = receipts
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source receipt missing"))?;
    let result = receipts
        .result
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("result receipt missing"))?;
    ensure!(
        !source.receipt_id.is_nil()
            && !result.receipt_id.is_nil()
            && source.receipt_id != result.receipt_id
            && source.attempt == stopped.key
            && result.attempt == stopped.key
            && result.artifact_digest == Digest::of(&stopped.evidence),
        "retention receipt scope/evidence mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn central_retention_receipts_require_exact_attempt_and_result_evidence() {
        let stopped = StoppedAttempt {
            key: AttemptKey {
                launch_id: Uuid::new_v4(),
                attempt_id: Uuid::new_v4(),
                manifest_digest: Digest::of(b"manifest"),
            },
            workspace: "/unused".into(),
            evidence: b"kernel stopped proof".to_vec(),
        };
        let source = CentralReceipt {
            receipt_id: Uuid::new_v4(),
            attempt: stopped.key.clone(),
            artifact_digest: Digest::of(b"retained source"),
        };
        let result = CentralReceipt {
            receipt_id: Uuid::new_v4(),
            attempt: stopped.key.clone(),
            artifact_digest: Digest::of(stopped.evidence()),
        };
        let valid = RetentionReceipts {
            source: Some(source),
            result: Some(result),
        };
        validate_receipts(&stopped, &valid).unwrap();
        let mut wrong = valid.clone();
        wrong.result.as_mut().unwrap().artifact_digest = Digest::of(b"other result");
        assert!(validate_receipts(&stopped, &wrong).is_err());
        let mut wrong = valid.clone();
        wrong.source.as_mut().unwrap().attempt.attempt_id = Uuid::new_v4();
        assert!(validate_receipts(&stopped, &wrong).is_err());
        let mut wrong = valid;
        wrong.result.as_mut().unwrap().receipt_id = wrong.source.as_ref().unwrap().receipt_id;
        assert!(validate_receipts(&stopped, &wrong).is_err());
    }
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod channels_tests;
#[cfg(test)]
mod confinement_tests;
#[cfg(test)]
mod native_probe_tests;

#[cfg(test)]
mod output_tests;
#[cfg(test)]
mod subscription_login_tests;
