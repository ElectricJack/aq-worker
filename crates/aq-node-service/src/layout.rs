//! Where a profile's configuration and per-slot state live.
use crate::{config::valid_profile, enrollment_file::write_private};
use anyhow::{Context, Result, bail, ensure};
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

pub struct Layout {
    pub config_dir: PathBuf,
    pub state_root: PathBuf,
}
fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
    Ok(PathBuf::from(home.context("HOME is not set")?).join(fallback))
}
fn private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))
}
impl Layout {
    pub fn for_profile(profile: &str) -> Result<Self> {
        ensure!(valid_profile(profile), "invalid profile name");
        let tail = Path::new("agent-q/service").join(profile);
        Ok(Self {
            config_dir: xdg("XDG_CONFIG_HOME", ".config")?.join(&tail),
            state_root: xdg("XDG_STATE_HOME", ".local/state")?.join(&tail),
        })
    }
    pub fn service_toml(&self) -> PathBuf {
        self.config_dir.join("service.toml")
    }
    pub fn slot_enrollment(&self, slot: u32) -> PathBuf {
        self.config_dir
            .join("slots")
            .join(slot.to_string())
            .join("worker.toml")
    }
    fn slot_state(&self, slot: u32) -> PathBuf {
        self.state_root.join("slots").join(slot.to_string())
    }
    pub fn current(&self, slot: u32) -> PathBuf {
        self.slot_state(slot).join("current")
    }
    pub fn archive_dir(&self, slot: u32) -> PathBuf {
        self.slot_state(slot).join("archive")
    }
    pub fn renderer_pin(&self) -> PathBuf {
        self.state_root.join("renderer.json")
    }
    pub fn machine_json(&self) -> PathBuf {
        self.config_dir.join("machine.json")
    }
    /// Creates current/{host,git} with mode 0700 (refuses if current exists).
    pub fn fresh_current(&self, slot: u32) -> Result<PathBuf> {
        private_dir(&self.slot_state(slot))?;
        let current = self.current(slot);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&current)
            .with_context(|| format!("create {}", current.display()))?;
        for d in ["host", "git"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(current.join(d))?;
        }
        Ok(current)
    }
    /// Wipes current/materials/*/material.bin, then renames current to
    /// archive/<UTC %Y%m%dT%H%M%SZ>-<4 hex>; writes EMPTY there when `empty`.
    pub fn archive_current(&self, slot: u32, empty: bool) -> Result<Option<PathBuf>> {
        let current = self.current(slot);
        if !current.exists() {
            return Ok(None);
        }
        if let Ok(materials) = std::fs::read_dir(current.join("materials")) {
            for entry in materials.flatten() {
                match std::fs::remove_file(entry.path().join("material.bin")) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => bail!("wipe material in {}: {e}", entry.path().display()),
                }
            }
        }
        let archive = self.archive_dir(slot);
        private_dir(&archive)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let tag = &uuid::Uuid::new_v4().simple().to_string()[..4];
        let target = archive.join(format!("{stamp}-{tag}"));
        std::fs::rename(&current, &target)
            .with_context(|| format!("archive {}", current.display()))?;
        if empty {
            std::fs::write(target.join("EMPTY"), b"")?;
        }
        Ok(Some(target))
    }
    /// Deletes archives that contain EMPTY; keeps everything else.
    pub fn prune_empty(&self, slot: u32) -> Result<u32> {
        let Ok(entries) = std::fs::read_dir(self.archive_dir(slot)) else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if entry.path().join("EMPTY").exists() {
                std::fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Writes {"key","label","kind":"local","host_load_file"} (0600, create-new then rename);
/// host_load_file is the Windows host-load file when `wsl`, else null. Label cut to 64 characters.
pub fn write_machine_json(path: &Path, key: &str, label: &str, wsl: bool) -> Result<()> {
    let label: String = label.chars().take(64).collect();
    let host_load_file = wsl.then_some("/mnt/c/ProgramData/agent-q/host-load.json");
    let body = serde_json::json!({"key": key, "label": label, "kind": "local", "host_load_file": host_load_file});
    write_private(path, &serde_json::to_vec_pretty(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn layout(dir: &Path) -> Layout {
        Layout {
            config_dir: dir.join("cfg"),
            state_root: dir.join("state"),
        }
    }
    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }
    #[test]
    fn fresh_current_is_private_and_unique() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let current = l.fresh_current(1).unwrap();
        assert_eq!(
            (
                mode(&current),
                mode(&current.join("host")),
                mode(&current.join("git"))
            ),
            (0o700, 0o700, 0o700)
        );
        assert!(l.fresh_current(1).is_err());
    }
    #[test]
    fn archive_wipes_material_and_marks_empty() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let current = l.fresh_current(2).unwrap();
        std::fs::create_dir_all(current.join("materials/m1")).unwrap();
        std::fs::write(current.join("materials/m1/material.bin"), b"login").unwrap();
        std::fs::write(current.join("journal"), b"j").unwrap();
        let kept = l.archive_current(2, false).unwrap().unwrap();
        assert!(!current.exists() && kept.join("journal").exists());
        assert!(!kept.join("materials/m1/material.bin").exists() && !kept.join("EMPTY").exists());
        l.fresh_current(2).unwrap();
        let empty = l.archive_current(2, true).unwrap().unwrap();
        assert!(empty.join("EMPTY").exists());
        assert_eq!(l.archive_current(2, true).unwrap(), None);
    }
    #[test]
    fn prune_deletes_only_empty_archives() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        l.fresh_current(1).unwrap();
        let kept = l.archive_current(1, false).unwrap().unwrap();
        l.fresh_current(1).unwrap();
        let empty = l.archive_current(1, true).unwrap().unwrap();
        assert_eq!(l.prune_empty(1).unwrap(), 1);
        assert!(kept.exists() && !empty.exists());
    }
    #[test]
    fn machine_json_is_a_machine_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine.json");
        write_machine_json(&path, "desk", &"x".repeat(80), true).unwrap();
        assert_eq!(mode(&path), 0o600);
        let c = aq_machine_load::MachineConfig::load_optional(&path).unwrap();
        assert_eq!(c.key.as_deref(), Some("desk"));
        assert_eq!(c.label.as_deref().map(str::len), Some(64));
        assert_eq!(
            c.host_load_file.as_deref(),
            Some(Path::new("/mnt/c/ProgramData/agent-q/host-load.json"))
        );
        write_machine_json(&path, "desk", "Desk", false).unwrap();
        assert!(
            aq_machine_load::MachineConfig::load_optional(&path)
                .unwrap()
                .host_load_file
                .is_none()
        );
    }
}
