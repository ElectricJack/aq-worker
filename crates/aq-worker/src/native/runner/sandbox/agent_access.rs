//! Scoped AQ access for the agent inside a sandbox (B7).
//!
//! The node issues the per-attempt agent credential (`/v2/worker/agent-session`
//! through `aq_agent_client::provision::Allocator`); the runner never calls
//! issuance. The runner only installs the delivered session files into the
//! agent's private state directory with the loopback AQ proxy (B6b) as the
//! endpoint, so the agent never holds the Access token. Provisioning stops
//! permanently on cancellation, expiry or revocation; the server stays
//! authoritative for all three.
use super::auth::RunnerHttp;
use anyhow::{Result, ensure};
use aq_agent_client::{
    client::ClientConfig,
    provision::{Installed, SessionDelivery, install_delivered, revoke_delivered},
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

/// The attempt grant an agent credential must stay inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentGrant {
    pub attempt_id: Uuid,
    pub node_id: Uuid,
    /// The earlier of the grant expiry and the hard deadline.
    pub not_after_ms: u64,
    /// False once the runner's permit authority is lost (cancel or revoke).
    pub live: bool,
}
impl AgentGrant {
    pub fn from_runner(http: &RunnerHttp) -> Self {
        let context = http.context();
        Self {
            attempt_id: context.binding.attempt_id,
            node_id: context.binding.node.node_id,
            not_after_ms: context.grant_expires_at_ms.min(context.hard_deadline_ms),
            live: !http.authority_lost(),
        }
    }
}

pub struct AgentAccess {
    state_dir: PathBuf,
    proxy_origin: String,
    stopped: AtomicBool,
}
impl AgentAccess {
    /// `state_dir` is the agent user's private state directory; `proxy_origin`
    /// is the B6b `AqApiProxy::origin()`.
    pub fn new(state_dir: PathBuf, proxy_origin: &str) -> Self {
        Self {
            state_dir,
            proxy_origin: proxy_origin.to_owned(),
            stopped: AtomicBool::new(false),
        }
    }
    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }
    /// Install (or rotate) the delivered credential for this attempt.
    pub fn provision(
        &self,
        delivery: &SessionDelivery,
        grant: &AgentGrant,
        now_ms: u64,
    ) -> Result<Installed> {
        ensure!(
            !self.stopped.load(Ordering::SeqCst),
            "agent access stopped; provisioning refused"
        );
        ensure!(grant.live, "attempt authority lost; provisioning refused");
        ensure!(
            now_ms < grant.not_after_ms,
            "attempt grant expired; provisioning refused"
        );
        let config: ClientConfig =
            serde_json::from_value(aq_agent_client::contract::parse(&delivery.client)?)?;
        ensure!(
            config.binding.attempt_id == grant.attempt_id
                && config.binding.node_id == grant.node_id,
            "delivered credential is bound to another attempt or node"
        );
        let installed = install_delivered(
            &self.state_dir,
            delivery,
            &self.proxy_origin,
            grant.not_after_ms,
            now_ms,
        )?;
        if self.stopped.load(Ordering::SeqCst) {
            // A concurrent stop() won the race; never leave a credential behind.
            revoke_delivered(&self.state_dir)?;
            anyhow::bail!("agent access stopped; provisioning refused");
        }
        Ok(installed)
    }
    /// Permanently stop provisioning and remove the installed credential.
    pub fn stop(&self) -> Result<()> {
        self.stopped.store(true, Ordering::SeqCst);
        revoke_delivered(&self.state_dir)
    }
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}
