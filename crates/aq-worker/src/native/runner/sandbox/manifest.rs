//! Private local handoff, produced only after validated phase preparation.
//! Deserialization checks shape and correlation; it does not authenticate authority.
use anyhow::{Context, Result, ensure};
use aq_admission_sandbox::{control::DeliveryStamp, sandbox::PhaseExecutionAuthority};
use aq_session_protocol::{LaunchManifest, Validate, canonical_digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenLaunch {
    pub schema_version: u32,
    pub launch_id: Uuid,
    pub manifest: LaunchManifest,
    pub authority: PhaseExecutionAuthority,
    pub state_dir: PathBuf,
    pub state_device: u64,
    pub state_inode: u64,
    pub kernel_boot_id: String,
    pub delivery: DeliveryStamp,
    pub sent_boot_ns: u64,
    pub received_boot_ns: u64,
    pub revision: u64,
}
impl FrozenLaunch {
    pub fn load(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() <= 4_194_304 && meta.permissions().mode() & 0o077 == 0,
            "private bounded regular handoff required"
        );
        let mut bytes = Vec::new();
        file.take(4_194_305).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4_194_304, "handoff grew beyond bound");
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub fn validate_local(&self, state: &Path) -> Result<(u64, u64)> {
        ensure!(
            self.schema_version == 1 && !self.launch_id.is_nil() && self.revision > 0,
            "handoff version/identity"
        );
        self.manifest.validate()?;
        self.authority.validate()?;
        ensure!(
            self.authority.grant.binding.session == self.manifest.bindings.session
                && self.authority.grant.binding.attempt_id == self.manifest.bindings.attempt_id
                && self.authority.grant.binding.assignment_generation
                    == self.manifest.bindings.assignment_generation
                && self.authority.grant.binding.graph_access_profile_digest
                    == self.manifest.bindings.graph_access_profile.digest,
            "phase/manifest binding"
        );
        ensure!(
            self.authority.grant.binding.launch_manifest_digest
                == canonical_digest(&self.manifest)?,
            "phase/manifest digest"
        );
        let meta = std::fs::symlink_metadata(state)?;
        ensure!(
            meta.is_dir()
                && meta.permissions().mode() & 0o077 == 0
                && state.canonicalize()? == self.state_dir
                && meta.dev() == self.state_device
                && meta.ino() == self.state_inode,
            "state directory identity/privacy"
        );
        ensure!(
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
                == self.kernel_boot_id,
            "handoff belongs to another kernel boot"
        );
        self.delivery
            .validate_for(self.delivery.nonce, self.authority.grant.expires_at_ms)?;
        ensure!(
            self.delivery.clock.server_time_ms >= self.authority.issued_at_ms,
            "historical delivery"
        );
        let hard = self
            .authority
            .hard_deadline_ms
            .checked_sub(self.delivery.clock.server_time_ms)
            .and_then(|v| v.checked_mul(1_000_000))
            .and_then(|v| v.checked_add(self.sent_boot_ns))
            .context("hard deadline overflow")?;
        let deadline = self.delivery.client_deadline_ns(
            self.sent_boot_ns,
            self.received_boot_ns,
            Some(hard),
        )?;
        let now = super::boot_ns()?;
        ensure!(
            now >= self.received_boot_ns && now < deadline,
            "expired/future local handoff"
        );
        // Never manufacture a fresh TTL when a retained manifest is read.
        Ok((deadline, hard))
    }
}
