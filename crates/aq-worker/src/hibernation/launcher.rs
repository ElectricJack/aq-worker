//! `SessionLauncher` shape for the common tmux host: fresh scoped credentials
//! for the successor grant are planted into the new sandbox, then exactly one
//! session starts with the bootstrap instructions.
//!
//! Both halves are ports. `SuccessorCredentials` stands for AQ's successor
//! ExecutionPermit / credential issuance, which has no route yet, and
//! `SessionHostStart` for `aq_session_host::SessionHost::launch`, whose permit
//! and `LaunchAuthority` come from that issuance (docs/implementation/H5.md,
//! "Port-level TODOs").
use anyhow::{Context, Result, bail};
use aq_lifecycle_protocol::Binding;
use std::collections::BTreeMap;
use std::path::Path;
use zeroize::Zeroizing;

use super::credentials::{LOGIN_CREDENTIALS, RUNNER_CREDENTIALS};
use super::{CredentialLayout, SessionLauncher};

/// Fresh credential files for one successor grant, keyed by their layout name
/// (`RUNNER_CREDENTIALS` under the runner root, `LOGIN_CREDENTIALS` under the
/// work root). Bytes are zeroized on drop and have no Debug.
#[derive(Default)]
pub struct IssuedCredentials {
    files: BTreeMap<&'static str, Zeroizing<Vec<u8>>>,
}
impl IssuedCredentials {
    /// Add one credential file; refuses names outside the credential layout.
    pub fn insert(&mut self, name: &str, bytes: Vec<u8>) -> Result<()> {
        let Some(known) = RUNNER_CREDENTIALS
            .iter()
            .chain(LOGIN_CREDENTIALS.iter())
            .find(|known| **known == name)
        else {
            bail!("{name} is not a sandbox credential path");
        };
        self.files.insert(known, Zeroizing::new(bytes));
        Ok(())
    }
}

/// TODO(platform follow-up): successor credential issuance. Must return
/// credentials scoped to `successor` only, never the predecessor's.
pub trait SuccessorCredentials {
    fn issue(&mut self, successor: &Binding) -> Result<IssuedCredentials>;
}

/// Start one fresh session for `successor` on the common host. Must be
/// idempotent per successor session: the continuation journals the launch
/// after it returns, so a crash in between replays this call.
pub trait SessionHostStart {
    fn start(&mut self, successor: &Binding, workspace: &Path, bootstrap: &Path) -> Result<()>;
}

pub struct FreshSessionLauncher<I, H> {
    pub issuer: I,
    pub host: H,
}
impl<I: SuccessorCredentials, H: SessionHostStart> SessionLauncher for FreshSessionLauncher<I, H> {
    fn launch(
        &mut self,
        successor: &Binding,
        layout: &CredentialLayout,
        bootstrap: &Path,
    ) -> Result<()> {
        if !bootstrap.is_file() {
            bail!("bootstrap instructions must precede session launch");
        }
        let issued = self.issuer.issue(successor)?;
        if issued.files.is_empty() {
            bail!("successor credential issuance returned nothing");
        }
        for (name, bytes) in &issued.files {
            let root = if RUNNER_CREDENTIALS.contains(name) {
                &layout.runner_root
            } else {
                &layout.work_root
            };
            let path = root.join(name);
            let parent = path.parent().context("credential parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            crate::guardian::durable_replace(&path, bytes)
                .with_context(|| format!("plant {name}"))?;
        }
        self.host.start(successor, &layout.workspace(), bootstrap)
    }
}
