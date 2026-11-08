//! `service.toml` (0600, owner-only).
use crate::enrollment_file::{read_private, write_private};
use anyhow::{Context, Result, bail, ensure};
use aq_protocol::{MAX_MACHINE_SLOTS, valid_machine_key};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub schema_version: u32,
    pub profile: String,
    pub machine_key: String,
    pub slots: u32,
    pub native_template: PathBuf,
    pub state_root: PathBuf,
    pub aq_git: AqGitConfig,
    #[serde(default)]
    pub repositories: Vec<RepositoryBinding>,
    #[serde(default)]
    pub measurement: MeasurementConfig,
    #[serde(default = "default_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AqGitConfig {
    pub binary: PathBuf,
    pub git_executable: PathBuf,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    pub repository_id: Uuid,
    pub identity: String,
    pub binding_generation: u64,
    pub object_format: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_renew_before_ms")]
    pub renew_before_ms: u64,
}
impl Default for MeasurementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_file: None,
            model: None,
            renew_before_ms: default_renew_before_ms(),
        }
    }
}
fn default_shutdown_grace_ms() -> u64 {
    120_000
}
fn default_renew_before_ms() -> u64 {
    14_400_000
}
/// `^[a-z0-9][a-z0-9-]{0,31}$`
pub fn valid_profile(p: &str) -> bool {
    (1..=32).contains(&p.len())
        && !p.starts_with('-')
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What systemd allows beyond the shutdown grace before it kills the service: the forced stop
/// and the measurement release fit inside it (asserted in the supervisor).
pub const STOP_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);
impl ServiceConfig {
    /// The unit's TimeoutStopSec: the grace, rounded up to seconds, plus `STOP_MARGIN`.
    pub fn stop_timeout_secs(&self) -> u64 {
        self.shutdown_grace_ms.div_ceil(1000) + STOP_MARGIN.as_secs()
    }
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&read_private(path)?)
            .with_context(|| format!("parse {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        write_private(
            path,
            toml::to_string(self)
                .context("serialize service config")?
                .as_bytes(),
        )
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported service config schema_version"
        );
        ensure!(valid_profile(&self.profile), "invalid profile name");
        ensure!(valid_machine_key(&self.machine_key), "invalid machine_key");
        ensure!(
            (1..=MAX_MACHINE_SLOTS).contains(&self.slots),
            "slots must be 1..={MAX_MACHINE_SLOTS}"
        );
        for (name, path) in [
            ("native_template", &self.native_template),
            ("state_root", &self.state_root),
            ("aq_git.binary", &self.aq_git.binary),
            ("aq_git.git_executable", &self.aq_git.git_executable),
        ] {
            ensure!(path.is_absolute(), "{name} must be an absolute path");
        }
        ensure!(self.repositories.len() <= 16, "at most 16 repositories");
        for r in &self.repositories {
            ensure!(
                !r.identity.is_empty() && r.identity.len() <= 128,
                "invalid repository identity"
            );
            ensure!(r.object_format == "sha1", "unsupported object_format");
        }
        ensure!(
            (10_000..=600_000).contains(&self.shutdown_grace_ms),
            "shutdown_grace_ms must be 10000..=600000"
        );
        let m = &self.measurement;
        ensure!(
            (3_600_000..=43_200_000).contains(&m.renew_before_ms),
            "renew_before_ms must be 3600000..=43200000"
        );
        if m.enabled {
            let Some(file) = &m.token_file else {
                bail!("measurement.token_file is required when measurement is enabled")
            };
            ensure!(
                file.is_absolute(),
                "measurement.token_file must be an absolute path"
            );
            ensure!(
                m.model.as_deref().is_some_and(|m| !m.is_empty()),
                "measurement.model is required when measurement is enabled"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> ServiceConfig {
        ServiceConfig {
            schema_version: 1,
            profile: "local".into(),
            machine_key: "desk".into(),
            slots: 2,
            native_template: "/etc/aq/native.json".into(),
            state_root: "/var/aq".into(),
            aq_git: AqGitConfig {
                binary: "/opt/aq-git".into(),
                git_executable: "/usr/bin/git".into(),
            },
            repositories: vec![],
            measurement: MeasurementConfig::default(),
            shutdown_grace_ms: 120_000,
        }
    }
    #[test]
    fn loads_valid_and_refuses_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        valid().save(&path).unwrap();
        assert_eq!(ServiceConfig::load(&path).unwrap().slots, 2);
        let bad: [fn(&mut ServiceConfig); 7] = [
            |c| c.slots = 0,
            |c| c.slots = 9,
            |c| c.native_template = "native.json".into(),
            |c| c.profile = "Bad_Name".into(),
            |c| c.machine_key = "Desk".into(),
            |c| c.measurement.enabled = true,
            |c| {
                c.repositories = vec![RepositoryBinding {
                    repository_id: Uuid::nil(),
                    identity: "x".into(),
                    binding_generation: 1,
                    object_format: "sha256".into(),
                }]
            },
        ];
        for edit in bad {
            let mut c = valid();
            edit(&mut c);
            assert!(c.validate().is_err());
        }
    }
    #[test]
    fn defaults_fill_grace_and_renew_window() {
        let text = r#"schema_version = 1
profile = "local"
machine_key = "desk"
slots = 1
native_template = "/n.json"
state_root = "/s"
[aq_git]
binary = "/g"
git_executable = "/usr/bin/git"
"#;
        let c: ServiceConfig = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(
            (c.shutdown_grace_ms, c.measurement.renew_before_ms),
            (120_000, 14_400_000)
        );
        assert_eq!(c.stop_timeout_secs(), 150, "the template's default");
        let longest = ServiceConfig {
            shutdown_grace_ms: 600_000,
            ..c.clone()
        };
        assert_eq!(longest.stop_timeout_secs(), 630);
        assert!(!c.measurement.enabled && c.repositories.is_empty());
    }
}
