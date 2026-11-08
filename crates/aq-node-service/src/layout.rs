//! Where a profile's configuration, per-slot state and runtime files live.
//!
//! Durable state stays under `state_root`. Every Unix socket, and every directory the session
//! host binds one in, lives under the short `runtime_root` instead: `sun_path` holds 107 bytes
//! and the session host refuses a tmux socket of 104 or more, which no XDG state path under a
//! normal home leaves room for.
use crate::{ServiceConfig, config::valid_profile, enrollment_file::write_private};
use anyhow::{Context, Result, bail, ensure};
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

pub struct Layout {
    pub config_dir: PathBuf,
    pub state_root: PathBuf,
    /// `$XDG_RUNTIME_DIR/aq/<profile>`, else `/tmp/aq-<uid>/<profile>`, unless service.toml
    /// sets `runtime_root`.
    pub runtime_root: PathBuf,
}
/// The longest socket path allowed: the session host's tmux check is `< 104`.
pub const MAX_SOCKET_PATH: usize = 103;
/// The sockets a slot's runtime directory holds, relative to it, with a launch id in place of
/// each `*`: aq-git's, and the session host's in a launch directory and a driver directory.
const SLOT_SOCKETS: [&str; 6] = [
    "git.sock",
    "host/*/tmux.sock",
    "host/*/observed-gate.sock",
    "host/*/terminal-reader.sock",
    "host/*/terminal-viewer.sock",
    "host/driver-*/tmux.sock",
];
/// Refuses a runtime root under which any slot's socket path would be too long, naming the
/// longest such path and its length.
pub fn check_socket_paths(runtime_root: &Path, slots: u32) -> Result<()> {
    let launch = uuid::Uuid::nil().to_string();
    let longest = (1..=slots)
        .flat_map(|slot| {
            let dir = runtime_root.join(slot.to_string());
            SLOT_SOCKETS.map(|s| dir.join(s.replace('*', &launch)))
        })
        .max_by_key(|p| p.as_os_str().len());
    if let Some(path) = longest {
        let len = path.as_os_str().len();
        ensure!(
            len <= MAX_SOCKET_PATH,
            "socket path {} would be {len} bytes, over the {MAX_SOCKET_PATH}-byte limit of Unix sockets; use a shorter profile name or set a shorter runtime_root in service.toml",
            path.display()
        );
    }
    Ok(())
}
/// `$XDG_RUNTIME_DIR/aq/<profile>` when XDG_RUNTIME_DIR is set (and absolute), else
/// `/tmp/aq-<uid>/<profile>`.
pub fn default_runtime_root(profile: &str) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir.join("aq").join(profile),
        _ => PathBuf::from(format!("/tmp/aq-{}", unsafe { libc::geteuid() })).join(profile),
    }
}
/// Creates `path` (0700) when missing, then requires a real directory (not a symlink) owned by
/// this user without any of the `refused` mode bits.
fn owned_dir(path: &Path, refused: u32) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).with_context(|| format!("create {}", path.display()));
        }
        _ => {}
    }
    let m = std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        m.file_type().is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & refused == 0,
        "{} must be a directory (not a symlink) owned by this user, mode {}",
        path.display(),
        if refused == 0o077 {
            "0700"
        } else {
            "0700 or 0755"
        }
    );
    Ok(())
}
fn owned_private_dir(path: &Path) -> Result<()> {
    owned_dir(path, 0o077)
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
            runtime_root: default_runtime_root(profile),
        })
    }
    /// The layout `service.toml` in `config_dir` describes: its state and runtime roots.
    pub fn for_service(config_dir: &Path, config: &ServiceConfig) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
            state_root: config.state_root.clone(),
            runtime_root: config.runtime_root(),
        }
    }
    /// Creates the runtime root and its parent when missing (0700) and checks both: owned by
    /// this user, the root private and the parent writable only by this user, and no symlink
    /// anywhere (the root is its own canonical path).
    pub fn prepare_runtime(&self) -> Result<()> {
        let parent = self
            .runtime_root
            .parent()
            .context("runtime_root has no parent")?;
        owned_dir(parent, 0o022)?;
        owned_private_dir(&self.runtime_root)?;
        ensure!(
            std::fs::canonicalize(&self.runtime_root)? == self.runtime_root,
            "runtime_root {} must not pass through a symlink",
            self.runtime_root.display()
        );
        Ok(())
    }
    /// A slot's runtime directory: `git.sock` and the session host root.
    pub fn slot_runtime(&self, slot: u32) -> PathBuf {
        self.runtime_root.join(slot.to_string())
    }
    pub fn git_socket(&self, slot: u32) -> PathBuf {
        self.slot_runtime(slot).join("git.sock")
    }
    /// The session host root of the slot's attempt: `current/host` for an attempt started by a
    /// build that kept it there (its journal and driver directories name that path), else
    /// `<slot runtime>/host`.
    pub fn host_root(&self, slot: u32) -> PathBuf {
        let legacy = self.current(slot).join("host");
        if legacy.is_dir() {
            legacy
        } else {
            self.slot_runtime(slot).join("host")
        }
    }
    /// The slot's runtime directory for recovery: created when missing (after a reboot), never
    /// emptied.
    pub fn slot_runtime_for_recovery(&self, slot: u32) -> Result<PathBuf> {
        self.prepare_runtime()?;
        let dir = self.slot_runtime(slot);
        owned_private_dir(&dir)?;
        Ok(dir)
    }
    fn remove_slot_runtime(&self, slot: u32) -> Result<()> {
        match std::fs::remove_dir_all(self.slot_runtime(slot)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("remove {}", self.slot_runtime(slot).display()))
            }
            _ => Ok(()),
        }
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
    pub fn slot_state(&self, slot: u32) -> PathBuf {
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
    /// Held while a qualification measurement runs (`measure::measure_lock`).
    pub fn measure_lock(&self) -> PathBuf {
        self.state_root.join("measure.lock")
    }
    pub fn machine_json(&self) -> PathBuf {
        self.config_dir.join("machine.json")
    }
    /// Creates current/git and a new, empty slot runtime directory with its host root, all
    /// 0700 (refuses if current exists). What an earlier attempt left in the runtime
    /// directory is removed: its `current` was archived.
    pub fn fresh_current(&self, slot: u32) -> Result<PathBuf> {
        private_dir(&self.slot_state(slot))?;
        let current = self.current(slot);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&current)
            .with_context(|| format!("create {}", current.display()))?;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(current.join("git"))?;
        self.prepare_runtime()?;
        self.remove_slot_runtime(slot)?;
        let runtime = self.slot_runtime(slot);
        for dir in [&runtime, &runtime.join("host")] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(current)
    }
    /// Wipes current/materials/*/material.bin, then renames current to
    /// archive/<UTC %Y%m%dT%H%M%SZ>-<4 hex>; writes EMPTY there when `empty`. The slot's
    /// runtime directory (sockets, and the host root with its leased files) is removed.
    pub fn archive_current(&self, slot: u32, empty: bool) -> Result<Option<PathBuf>> {
        let current = self.current(slot);
        if !current.exists() {
            self.remove_slot_runtime(slot)?;
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
        self.remove_slot_runtime(slot)?;
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
            runtime_root: dir.join("run/p"),
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
                mode(&l.host_root(1)),
                mode(&current.join("git")),
                mode(&l.slot_runtime(1)),
                mode(&l.runtime_root),
            ),
            (0o700, 0o700, 0o700, 0o700, 0o700)
        );
        assert_eq!(l.host_root(1), dir.path().join("run/p/1/host"));
        assert_eq!(l.git_socket(1), dir.path().join("run/p/1/git.sock"));
        assert!(
            !current.join("host").exists(),
            "no socket directory in the state"
        );
        assert!(l.fresh_current(1).is_err());
    }
    /// The runtime directory belongs to `current`: archiving removes it, a fresh attempt
    /// starts with an empty one, and recovery recreates a lost one without emptying it.
    #[test]
    fn runtime_dir_follows_current() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        l.fresh_current(1).unwrap();
        std::fs::write(l.host_root(1).join("leftover"), b"").unwrap();
        l.archive_current(1, true).unwrap().unwrap();
        assert!(!l.slot_runtime(1).exists());
        std::fs::create_dir_all(l.host_root(1)).unwrap();
        std::fs::write(l.host_root(1).join("leftover"), b"").unwrap();
        l.fresh_current(1).unwrap();
        assert!(!l.host_root(1).join("leftover").exists());
        std::fs::write(l.host_root(1).join("kept"), b"").unwrap();
        l.slot_runtime_for_recovery(1).unwrap();
        assert!(l.host_root(1).join("kept").exists());
        std::fs::remove_dir_all(l.slot_runtime(1)).unwrap();
        l.slot_runtime_for_recovery(1).unwrap();
        assert!(l.slot_runtime(1).is_dir() && !l.host_root(1).exists());
        // An attempt of the earlier layout keeps its host root in current.
        std::fs::create_dir(l.current(1).join("host")).unwrap();
        assert_eq!(l.host_root(1), l.current(1).join("host"));
    }
    /// The runtime root is refused when it, or its parent, is a symlink, another mode or not
    /// a directory.
    #[test]
    fn runtime_root_must_be_private_and_real() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        l.prepare_runtime().unwrap();
        std::fs::set_permissions(&l.runtime_root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = l.prepare_runtime().unwrap_err().to_string();
        assert!(e.contains("mode 0700"), "{e}");
        std::fs::remove_dir(&l.runtime_root).unwrap();
        std::fs::create_dir(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &l.runtime_root).unwrap();
        let e = l.prepare_runtime().unwrap_err().to_string();
        assert!(e.contains("not a symlink"), "{e}");
        std::fs::remove_file(&l.runtime_root).unwrap();
        std::fs::set_permissions(
            dir.path().join("run"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(l.prepare_runtime().is_err(), "a parent others can write");
        std::fs::set_permissions(
            dir.path().join("run"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let moved = Layout {
            runtime_root: dir.path().join("link/p"),
            ..layout(dir.path())
        };
        std::os::unix::fs::symlink(dir.path().join("run"), dir.path().join("link")).unwrap();
        assert!(moved.prepare_runtime().is_err(), "a symlinked parent");
    }
    #[test]
    fn default_runtime_root_is_short_and_per_profile() {
        let root = default_runtime_root("ms1");
        assert!(
            root.ends_with("aq/ms1")
                || root.ends_with(format!("aq-{}/ms1", unsafe { libc::geteuid() }))
        );
        check_socket_paths(&root, 8).unwrap();
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
