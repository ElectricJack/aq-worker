//! The existing host owns private tmux creation and all descendant stop proof.
//! Pane/EOF/server disappearance is never promoted to stopped-writer evidence.
use anyhow::Result;
use aq_session_host::{
    ExecutionPermit, HarnessAdapter, LaunchAuthority, LaunchIntent, LocalEnvironment,
    SessionHandle, SessionHost, StoppedProof,
};
use aq_session_protocol::LaunchManifest;
use std::{path::Path, time::Duration};
use uuid::Uuid;

pub(crate) fn launch(
    root: &Path,
    id: Uuid,
    manifest: &LaunchManifest,
    permit: ExecutionPermit,
    local: &LocalEnvironment,
    adapter: &impl HarnessAdapter,
    authority: &mut impl LaunchAuthority,
) -> Result<SessionHandle> {
    SessionHost::launch(root, id, manifest, permit, local, adapter, authority)
}
pub(crate) fn stop(handle: &mut SessionHandle) -> Result<StoppedProof> {
    handle.stop()?;
    handle.wait_stopped(Duration::from_secs(10))?;
    handle.stopped_proof()
}
pub(crate) fn recover_stopped(root: &Path, intent: &LaunchIntent) -> Result<StoppedProof> {
    // Recovery only requests stop. It cannot start or renew the retained host.
    let mut handle = SessionHost::recover(root, intent)?;
    stop(&mut handle)
}
