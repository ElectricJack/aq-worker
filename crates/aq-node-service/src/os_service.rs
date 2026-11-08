//! The OS service layer: systemd user units on Linux (spec 0007 §7.2). macOS launchd is reserved
//! for phase 5. Nothing here runs systemctl except through a `HostProbe`.
use crate::enrollment_file::write_private;
use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};

pub struct UnitSpec {
    pub profile: String,
    pub aq_worker: PathBuf,
    pub service_toml: PathBuf,
    pub requires_sandbox_unit: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ServiceState {
    Active,
    Inactive,
    Failed,
    NotInstalled,
    Other(String),
}
pub trait ServiceManager {
    fn kind(&self) -> &'static str;
    /// Writes the unit, reloads, and enables `aq-node@<profile>.service` now.
    fn install(&self, spec: &UnitSpec) -> Result<()>;
    /// Disables and stops the instance; the template stays while other instances use it.
    fn uninstall(&self, profile: &str) -> Result<()>;
    fn start(&self, profile: &str) -> Result<()>;
    fn stop(&self, profile: &str) -> Result<()>;
    fn status(&self, profile: &str) -> Result<ServiceState>;
}
pub trait HostProbe {
    fn read(&self, path: &str) -> Option<String>;
    /// Exit code and the combined trimmed output.
    fn run(&self, argv: &[&str]) -> Result<(i32, String)>;
}
pub struct RealHost;
impl HostProbe for RealHost {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn run(&self, argv: &[&str]) -> Result<(i32, String)> {
        let (program, args) = argv.split_first().context("empty command")?;
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("run {program}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok((out.status.code().unwrap_or(-1), text.trim().to_owned()))
    }
}

#[derive(Debug)]
pub struct Preflight {
    pub wsl: bool,
    pub linger: bool,
}
/// Linux only. Errors when PID 1 is not systemd (on WSL with the wsl.conf instruction) or the
/// user manager is offline. `linger` says whether the service survives logout.
pub fn preflight(host: &dyn HostProbe) -> Result<Preflight> {
    let wsl = host
        .read("/proc/sys/kernel/osrelease")
        .is_some_and(|r| r.to_lowercase().contains("microsoft"));
    if host
        .read("/proc/1/comm")
        .is_none_or(|c| c.trim() != "systemd")
    {
        if wsl {
            bail!(
                "WSL2 needs systemd: add\n[boot]\nsystemd=true\nto /etc/wsl.conf, then run `wsl --shutdown` from Windows"
            );
        }
        bail!(
            "systemd is not PID 1 on this machine; the worker service needs a systemd user manager"
        );
    }
    let (code, out) = host.run(&["systemctl", "--user", "is-system-running"])?;
    if code == 1 && out.contains("offline") {
        bail!(
            "the systemd user manager is offline; log in with a full session or run `loginctl enable-linger $USER`"
        );
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let linger = host
        .run(&["loginctl", "show-user", &user, "-p", "Linger", "--value"])
        .is_ok_and(|(code, out)| code == 0 && out == "yes");
    Ok(Preflight { wsl, linger })
}

const TEMPLATE: &str = "aq-node@.service";
pub struct SystemdUser<H: HostProbe> {
    pub unit_dir: PathBuf,
    pub host: H,
}
/// The template unit; `%i` is the profile.
pub fn render_unit(spec: &UnitSpec) -> String {
    let (requires, after) = if spec.requires_sandbox_unit {
        (
            "Requires=aq-sandbox-podman.service\n",
            "aq-sandbox-podman.service network-online.target",
        )
    } else {
        ("", "network-online.target")
    };
    format!(
        "# AQ worker service (spec 0007 §7.2). Installed by `aq-worker service install`.\n\
[Unit]\n\
Description=AQ worker service (%i)\n\
{requires}After={after}\n\
StartLimitIntervalSec=0\n\
\n\
[Service]\n\
Type=exec\n\
ExecStart={} run --service-config %h/.config/agent-q/service/%i/service.toml\n\
Restart=always\n\
RestartSec=10\n\
KillMode=mixed\n\
TimeoutStopSec=150\n\
Environment=RUST_LOG=info\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        spec.aq_worker.display()
    )
}
impl<H: HostProbe> SystemdUser<H> {
    fn systemctl(&self, args: &[&str]) -> Result<String> {
        let argv: Vec<&str> = ["systemctl", "--user"]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        let (code, out) = self.host.run(&argv)?;
        ensure!(
            code == 0,
            "systemctl {} failed ({code}): {out}",
            args.join(" ")
        );
        Ok(out)
    }
    fn instance(profile: &str) -> Result<String> {
        ensure!(
            crate::config::valid_profile(profile),
            "invalid profile name"
        );
        Ok(format!("aq-node@{profile}.service"))
    }
    fn others_enabled(&self) -> bool {
        std::fs::read_dir(self.unit_dir.join("default.target.wants")).is_ok_and(|d| {
            d.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("aq-node@") && n.ends_with(".service")
            })
        })
    }
}
impl<H: HostProbe> ServiceManager for SystemdUser<H> {
    fn kind(&self) -> &'static str {
        "systemd-user"
    }
    fn install(&self, spec: &UnitSpec) -> Result<()> {
        let instance = Self::instance(&spec.profile)?;
        ensure!(
            spec.aq_worker.is_absolute(),
            "the aq-worker path must be absolute"
        );
        write_unit(&self.unit_dir.join(TEMPLATE), render_unit(spec).as_bytes())?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", "--now", &instance]).map(drop)
    }
    fn uninstall(&self, profile: &str) -> Result<()> {
        let instance = Self::instance(profile)?;
        self.systemctl(&["disable", "--now", &instance])?;
        if !self.others_enabled() {
            match std::fs::remove_file(self.unit_dir.join(TEMPLATE)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("remove the unit template"),
            }
            self.systemctl(&["daemon-reload"])?;
        }
        Ok(())
    }
    fn start(&self, profile: &str) -> Result<()> {
        self.systemctl(&["start", &Self::instance(profile)?])
            .map(drop)
    }
    fn stop(&self, profile: &str) -> Result<()> {
        self.systemctl(&["stop", &Self::instance(profile)?])
            .map(drop)
    }
    fn status(&self, profile: &str) -> Result<ServiceState> {
        let instance = Self::instance(profile)?;
        if !self.unit_dir.join(TEMPLATE).exists() {
            return Ok(ServiceState::NotInstalled);
        }
        let (_, out) = self
            .host
            .run(&["systemctl", "--user", "is-active", &instance])?;
        Ok(match out.as_str() {
            "active" => ServiceState::Active,
            "inactive" => ServiceState::Inactive,
            "failed" => ServiceState::Failed,
            other => ServiceState::Other(other.to_owned()),
        })
    }
}
/// A unit file is world-readable by convention, but written through the same private rename.
fn write_unit(path: &Path, body: &[u8]) -> Result<()> {
    write_private(path, body)
}

/// The service manager of this OS.
#[cfg(target_os = "linux")]
pub fn detect() -> Result<Box<dyn ServiceManager>> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .context("HOME is not set")?;
    Ok(Box::new(SystemdUser {
        unit_dir: base.join("systemd/user"),
        host: RealHost,
    }))
}
#[cfg(target_os = "macos")]
pub fn detect() -> Result<Box<dyn ServiceManager>> {
    bail!("launchd service is spec 0007 phase 5")
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn detect() -> Result<Box<dyn ServiceManager>> {
    bail!("this operating system has no worker service layer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    #[derive(Default)]
    struct FakeHost {
        files: HashMap<&'static str, &'static str>,
        replies: HashMap<&'static str, (i32, &'static str)>,
        ran: RefCell<Vec<Vec<String>>>,
    }
    impl HostProbe for FakeHost {
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).map(|s| s.to_string())
        }
        fn run(&self, argv: &[&str]) -> Result<(i32, String)> {
            self.ran
                .borrow_mut()
                .push(argv.iter().map(|s| s.to_string()).collect());
            let key = if argv[0] == "loginctl" {
                "loginctl"
            } else {
                argv.get(2).copied().unwrap_or("")
            };
            let (code, out) = self.replies.get(key).copied().unwrap_or((0, ""));
            Ok((code, out.to_owned()))
        }
    }
    fn spec(requires: bool) -> UnitSpec {
        UnitSpec {
            profile: "local".into(),
            aq_worker: "/opt/aq/aq-worker".into(),
            service_toml: "/c/service.toml".into(),
            requires_sandbox_unit: requires,
        }
    }

    #[test]
    fn unit_renders_exactly() {
        let golden =
            "# AQ worker service (spec 0007 §7.2). Installed by `aq-worker service install`.
[Unit]
Description=AQ worker service (%i)
Requires=aq-sandbox-podman.service
After=aq-sandbox-podman.service network-online.target
StartLimitIntervalSec=0

[Service]
Type=exec
ExecStart=/opt/aq/aq-worker run --service-config %h/.config/agent-q/service/%i/service.toml
Restart=always
RestartSec=10
KillMode=mixed
TimeoutStopSec=150
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
";
        assert_eq!(render_unit(&spec(true)), golden);
        let bare = render_unit(&spec(false));
        assert!(
            bare.contains("After=network-online.target\n")
                && !bare.contains("Requires=")
                && !bare.contains("podman")
        );
    }
    #[test]
    fn preflight_explains_wsl_without_systemd() {
        let host = FakeHost {
            files: [
                ("/proc/1/comm", "init\n"),
                (
                    "/proc/sys/kernel/osrelease",
                    "5.15.0-Microsoft-standard-WSL2\n",
                ),
            ]
            .into(),
            ..Default::default()
        };
        let e = preflight(&host).unwrap_err().to_string();
        assert!(
            e.contains("[boot]\nsystemd=true") && e.contains("wsl --shutdown"),
            "{e}"
        );
        let plain = FakeHost {
            files: [("/proc/1/comm", "init\n")].into(),
            ..Default::default()
        };
        assert!(!preflight(&plain).unwrap_err().to_string().contains("wsl"));
        let offline = FakeHost {
            files: [("/proc/1/comm", "systemd\n")].into(),
            replies: [("is-system-running", (1, "offline"))].into(),
            ..Default::default()
        };
        assert!(
            preflight(&offline)
                .unwrap_err()
                .to_string()
                .contains("offline")
        );
    }
    #[test]
    fn preflight_reports_linger() {
        let files = [
            ("/proc/1/comm", "systemd\n"),
            ("/proc/sys/kernel/osrelease", "6.1.0-microsoft\n"),
        ];
        let on = FakeHost {
            files: files.into(),
            replies: [("loginctl", (0, "yes"))].into(),
            ..Default::default()
        };
        let p = preflight(&on).unwrap();
        assert!(p.wsl && p.linger);
        let off = FakeHost {
            files: files.into(),
            replies: [("loginctl", (0, "no"))].into(),
            ..Default::default()
        };
        assert!(!preflight(&off).unwrap().linger);
    }
    #[test]
    fn install_runs_daemon_reload_then_enable_now() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SystemdUser {
            unit_dir: dir.path().join("systemd/user"),
            host: FakeHost::default(),
        };
        manager.install(&spec(true)).unwrap();
        assert_eq!(
            *manager.host.ran.borrow(),
            [
                vec!["systemctl", "--user", "daemon-reload"],
                vec![
                    "systemctl",
                    "--user",
                    "enable",
                    "--now",
                    "aq-node@local.service"
                ]
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("systemd/user/aq-node@.service")).unwrap(),
            render_unit(&spec(true))
        );
        let failing = SystemdUser {
            unit_dir: dir.path().join("u2"),
            host: FakeHost {
                replies: [("daemon-reload", (1, "boom"))].into(),
                ..Default::default()
            },
        };
        assert!(
            failing
                .install(&spec(true))
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        assert!(
            manager
                .install(&UnitSpec {
                    profile: "Bad".into(),
                    ..spec(true)
                })
                .is_err()
        );
    }
    #[test]
    fn uninstall_keeps_the_template_while_other_instances_exist() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SystemdUser {
            unit_dir: dir.path().to_path_buf(),
            host: FakeHost::default(),
        };
        manager.install(&spec(false)).unwrap();
        std::fs::create_dir_all(dir.path().join("default.target.wants")).unwrap();
        std::fs::write(
            dir.path()
                .join("default.target.wants/aq-node@other.service"),
            b"",
        )
        .unwrap();
        manager.uninstall("local").unwrap();
        assert!(dir.path().join(TEMPLATE).exists());
        std::fs::remove_file(
            dir.path()
                .join("default.target.wants/aq-node@other.service"),
        )
        .unwrap();
        manager.uninstall("other").unwrap();
        assert!(!dir.path().join(TEMPLATE).exists());
    }
    #[test]
    fn status_maps_is_active_output() {
        let dir = tempfile::tempdir().unwrap();
        let status = |reply: &'static str| {
            let manager = SystemdUser {
                unit_dir: dir.path().to_path_buf(),
                host: FakeHost {
                    replies: [("is-active", (3, reply))].into(),
                    ..Default::default()
                },
            };
            manager.status("local").unwrap()
        };
        assert_eq!(
            status("active"),
            ServiceState::NotInstalled,
            "no template yet"
        );
        std::fs::write(dir.path().join(TEMPLATE), b"").unwrap();
        assert_eq!(status("active"), ServiceState::Active);
        assert_eq!(status("inactive"), ServiceState::Inactive);
        assert_eq!(status("failed"), ServiceState::Failed);
        assert_eq!(
            status("activating"),
            ServiceState::Other("activating".into())
        );
    }
}
