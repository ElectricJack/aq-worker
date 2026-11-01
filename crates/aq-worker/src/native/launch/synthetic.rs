//! Explicit synthetic installation only; never selected from an offer.
use super::*;
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalCommand,
    observed::{ObservedAdapterQualification, ObservedHarnessAdapter},
};
use aq_session_protocol::{Capability, HarnessSettings};
use sha2::Digest as _;
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticMode {
    Wait,
    ExitImmediately,
}
#[derive(Clone, Serialize)]
pub struct InstalledFilePin {
    pub path: PathBuf,
    pub artifact: ArtifactRef,
}
#[derive(Clone, Serialize)]
pub struct SyntheticHostConfig {
    pub root: PathBuf,
    pub helper: InstalledFilePin,
    pub tmux: InstalledFilePin,
    pub program: InstalledFilePin,
    pub harness: HarnessSettings,
    pub capabilities: Vec<Capability>,
    pub renderer: ArtifactRef,
    pub columns: u16,
    pub rows: u16,
    pub mode: SyntheticMode,
}
pub struct SyntheticHostProfile {
    pub(super) config: SyntheticHostConfig,
    pub(super) root: RootGuard,
    pub(super) digest: Digest,
}
pub(super) struct RootGuard {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl RootGuard {
    pub(super) fn open(path: &Path, expected: Option<(u64, u64)>) -> Result<Self> {
        ensure!(
            path.is_absolute()
                && path.as_os_str().len() <= 2048
                && std::fs::canonicalize(path)? == path,
            "synthetic root must be canonical"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let m = file.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
            "synthetic root must be private owner0700"
        );
        if let Some((dev, ino)) = expected {
            ensure!(m.dev() == dev && m.ino() == ino, "synthetic root replaced");
        }
        Ok(Self {
            file,
            path: path.into(),
            device: m.dev(),
            inode: m.ino(),
        })
    }
    pub(super) fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
    pub(super) fn verify(&self) -> Result<()> {
        let saved = self.file.metadata()?;
        let now = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            saved.is_dir()
                && now.is_dir()
                && saved.dev() == self.device
                && saved.ino() == self.inode
                && now.dev() == self.device
                && now.ino() == self.inode
                && now.uid() == unsafe { libc::geteuid() }
                && now.mode() & 0o777 == 0o700,
            "synthetic root namespace changed"
        );
        Ok(())
    }
}
impl SyntheticHostProfile {
    #[cfg(test)]
    pub(crate) fn config_for_test(&self) -> SyntheticHostConfig {
        self.config.clone()
    }
    pub fn new(config: SyntheticHostConfig) -> Result<Self> {
        ensure!(
            config.program.artifact.id.as_str() == "aq.synthetic-native-harness"
                && config.program.artifact.version.as_str() == "1"
                && config.capabilities.len() <= 64,
            "closed synthetic program/capability profile"
        );
        ensure!(
            config.columns > 0
                && config.columns <= 240
                && config.rows > 0
                && config.rows <= 120
                && u32::from(config.columns) * u32::from(config.rows) <= 12000,
            "synthetic geometry bound"
        );
        for pin in [&config.helper, &config.tmux, &config.program] {
            pin.artifact.id.validate()?;
            pin.artifact.version.validate()?;
            ensure!(
                pin.path.is_absolute()
                    && pin.path.as_os_str().len() <= 2048
                    && std::fs::canonicalize(&pin.path)? == pin.path,
                "synthetic installed path canonical"
            );
        }
        aq_session_protocol::require_capabilities(&[], &config.capabilities)?;
        aq_session_protocol::require_capabilities(&[], &config.harness.required_capabilities)?;
        for capability in &config.capabilities {
            capability.validate()?;
        }
        config.renderer.id.validate()?;
        config.renderer.version.validate()?;
        let root = RootGuard::open(&config.root, None)?;
        let digest = crate::native::journal::canonical_digest(&config)?;
        Ok(Self {
            config,
            root,
            digest,
        })
    }
    pub(super) fn verify_installation(&self) -> Result<()> {
        self.root.verify()?;
        // These helper/tmux paths are explicitly trusted operator installations.
        // Rehashing detects changes now; it does not claim immutability after check.
        for pin in [&self.config.helper, &self.config.tmux] {
            let mut f = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&pin.path)?;
            let m = f.metadata()?;
            ensure!(
                m.is_file() && m.len() > 0 && m.len() <= 64 * 1024 * 1024,
                "synthetic installed executable bound"
            );
            let mut hash = sha2::Sha256::new();
            let mut scratch = [0u8; 65536];
            let mut total = 0u64;
            loop {
                let n = f.read(&mut scratch)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                ensure!(
                    total <= 64 * 1024 * 1024,
                    "synthetic installed executable grew"
                );
                sha2::Digest::update(&mut hash, &scratch[..n]);
            }
            ensure!(
                format!("{:x}", sha2::Digest::finalize(hash)) == pin.artifact.digest.as_str()
                    && f.metadata()?.len() == total,
                "synthetic installed executable changed"
            );
        }
        Ok(())
    }
}
pub(super) struct Adapter {
    pub(super) profile: Arc<SyntheticHostProfile>,
    pub(super) launch_id: Uuid,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &aq_session_protocol::LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        ensure!(
            crate::native::journal::canonical_digest(&m.harness)?
                == crate::native::journal::canonical_digest(&self.profile.config.harness)?,
            "synthetic installed harness mismatch"
        );
        let directory = self.profile.config.root.join(self.launch_id.to_string());
        Ok(AdapterLaunch {
            manifest_digest: crate::native::journal::canonical_digest(m)?,
            adapter: self.profile.config.harness.adapter.clone(),
            cli_version: self.profile.config.harness.cli_version.as_str().into(),
            environment: self.profile.config.harness.environment.clone(),
            capabilities: self.profile.config.capabilities.clone(),
            resource_qualification: None,
            command: LocalCommand {
                executable: self.profile.config.program.path.clone(),
                arguments: vec![
                    directory
                        .join("context-0.bin")
                        .to_str()
                        .context("synthetic context path")?
                        .into(),
                    directory
                        .join("fixture-marker")
                        .to_str()
                        .context("synthetic marker path")?
                        .into(),
                    match self.profile.config.mode {
                        SyntheticMode::Wait => "wait",
                        SyntheticMode::ExitImmediately => "exit",
                    }
                    .into(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl ObservedHarnessAdapter for Adapter {
    fn observed_qualification(
        &self,
        _: &aq_session_protocol::LaunchManifest,
    ) -> Result<ObservedAdapterQualification> {
        Ok(ObservedAdapterQualification {
            executable: self.profile.config.program.artifact.clone(),
            renderer: self.profile.config.renderer.clone(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config(root: &Path) -> SyntheticHostConfig {
        let path = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let reference = |id: &str| ArtifactRef {
            id: aq_session_protocol::Text::new(id).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(b"metadata-only test, never executable qualification"),
        };
        let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
        ))
        .unwrap();
        SyntheticHostConfig {
            root: root.into(),
            helper: InstalledFilePin {
                path: path.clone(),
                artifact: reference("aq.synthetic-host-helper"),
            },
            tmux: InstalledFilePin {
                path: path.clone(),
                artifact: reference("aq.synthetic-system-tmux34"),
            },
            program: InstalledFilePin {
                path,
                artifact: reference("aq.synthetic-native-harness"),
            },
            harness: serde_json::from_value(
                fixture["proposal"]["original"]["offered"]["offer"]["manifest"]["harness"].clone(),
            )
            .unwrap(),
            capabilities: vec![],
            renderer: reference("aq.synthetic-baseline"),
            columns: 80,
            rows: 24,
            mode: SyntheticMode::Wait,
        }
    }
    #[test]
    fn synthetic_profile_rejects_offer_commands_geometry_and_nonprivate_or_replaced_roots() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("host");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let good = config(&root);
        let mut bad = good.clone();
        bad.program.artifact.id =
            aq_session_protocol::Text::new("arbitrary-offer-command").unwrap();
        assert!(SyntheticHostProfile::new(bad).is_err());
        let mut bad = good.clone();
        bad.columns = 240;
        bad.rows = 120;
        assert!(SyntheticHostProfile::new(bad).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SyntheticHostProfile::new(good.clone()).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = SyntheticHostProfile::new(good).unwrap();
        std::fs::rename(&root, dir.path().join("old-host")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(profile.root.verify().is_err());
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    }
}
