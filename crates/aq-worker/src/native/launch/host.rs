use super::*;
impl LocalPhaseHost {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
    pub fn observe(&self) -> Result<HostObservation> {
        lock(&self.entry.physical)?
            .handle
            .as_ref()
            .context("native physical handle absent")?
            .observe()
    }
    /// Extend the host guardian's local permit (never server authority) up to
    /// `until_boot_ms`, which the caller bounds by the server grant. Used by
    /// non-interactive hosts whose local permit is capped at MAX_PERMIT_MS.
    pub fn renew_local(&self, sequence: u64, until_boot_ms: u64) -> Result<()> {
        let intent = lock(&self.entry.history)?
            .as_ref()
            .map(|h| h.intent.clone())
            .context("native launch intent absent")?;
        let now = boot_ns()? / 1_000_000;
        let permit = ExecutionPermit {
            session: intent.binding.session.clone(),
            manifest_digest: intent.binding.launch_manifest_digest.clone(),
            sequence,
            issued_boot_ms: now,
            expires_boot_ms: until_boot_ms
                .min(now + aq_session_host::MAX_PERMIT_MS - 5_000)
                .min(intent.hard_deadline_ns.min(intent.deadline_ns) / 1_000_000),
        };
        lock(&self.entry.physical)?
            .handle
            .as_ref()
            .context("native physical handle absent")?
            .renew(&permit)
    }
    /// MS1 docker-local: the driver's own exit record (`exit.json` in its
    /// private directory), written before it submits the claim. None when the
    /// host is not a Claude docker-local host or the record is absent.
    pub fn claude_driver_succeeded(&self) -> Option<bool> {
        driver_exit_success(&self.claude_driver_dir()?.join("exit.json"))
    }
    /// MS1 docker-local: the driver's private directory, if this is a Claude
    /// docker-local host.
    pub fn claude_driver_dir(&self) -> Option<std::path::PathBuf> {
        if !matches!(self.entry.profile, Some(Profile::ClaudeDocker(_))) {
            return None;
        }
        let intent = lock(&self.entry.history).ok()?.as_ref()?.intent.clone();
        Some(claude_driver_dir(&intent.host_root, intent.launch_id))
    }
}
/// `<host_root>/driver-<launch_id>`: the private directory of one launch.
pub fn claude_driver_dir(host_root: &std::path::Path, launch_id: Uuid) -> std::path::PathBuf {
    host_root.join(format!("driver-{launch_id}"))
}
/// MS1 review S3a: after the host stopped, the launch's leased token file is
/// removed whatever happened to the driver (it normally deleted it itself).
pub fn wipe_claude_driver_secrets(dir: &std::path::Path) {
    match std::fs::remove_file(dir.join("token")) {
        Ok(()) => tracing::warn!(dir = %dir.display(), "removed a leftover leased token file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %dir.display(), "leased token file not removed: {e}"),
    }
}
impl LocalPhaseHost {
    pub fn started_request(&self) -> Result<aq_admission::started::Request> {
        lock(&self.entry.history)?
            .as_ref()
            .and_then(|h| h.running.as_ref())
            .map(|r| r.request.clone())
            .context("native Running history absent")
    }
}
impl HeldLaunchHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
}
impl LocalStoppedHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
}
