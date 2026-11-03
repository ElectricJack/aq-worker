//! Linux identity transitions for a dedicated sandbox runner and its pane.
//! This is file/user separation, not hostile-code isolation on a shared kernel.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{os::unix::fs::MetadataExt, path::Path};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    pub uid: u32,
    pub gid: u32,
}
const RUNNER_CAPS: u32 = (1 << 5) | (1 << 6) | (1 << 7); // KILL, SETGID, SETUID
#[repr(C)]
struct Header {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Caps {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
fn syscall_result(ok: bool) -> Result<()> {
    ensure!(
        ok,
        "sandbox identity transition refused: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn set_caps(data: &mut [Caps; 2]) -> Result<()> {
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    syscall_result(unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_mut_ptr()) } == 0)
}
fn caps() -> Result<[Caps; 2]> {
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [Caps::default(); 2];
    syscall_result(
        unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } == 0,
    )?;
    Ok(data)
}
fn single_threaded() -> Result<()> {
    ensure!(
        std::fs::read_dir("/proc/self/task")?.count() == 1,
        "identity transition requires fresh single-thread process"
    );
    Ok(())
}
impl AgentIdentity {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.uid > 0 && self.uid != u32::MAX && self.gid > 0 && self.gid != u32::MAX,
            "nonroot sandbox identity required"
        );
        Ok(())
    }
    /// Invoke only in the dedicated pane immediately before exec. A failed
    /// operation returns an error; callers must never execute a fallback.
    pub fn drop_privileges(self) -> Result<()> {
        self.validate()?;
        single_threaded()?;
        ensure!(
            unsafe { libc::geteuid() } != self.uid,
            "agent and runner must use different UIDs"
        );
        syscall_result(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0)?;
        syscall_result(unsafe { libc::setgroups(0, std::ptr::null()) } == 0)?;
        syscall_result(unsafe { libc::setgid(self.gid) } == 0)?;
        syscall_result(unsafe { libc::setuid(self.uid) } == 0)?;
        syscall_result(
            unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_CLEAR_ALL,
                    0,
                    0,
                    0,
                )
            } == 0,
        )?;
        set_caps(&mut [Caps::default(); 2])?;
        syscall_result(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) } == 0)?;
        ensure!(
            unsafe { libc::getuid() } == self.uid
                && unsafe { libc::geteuid() } == self.uid
                && unsafe { libc::getgid() } == self.gid
                && unsafe { libc::getegid() } == self.gid
                && unsafe { libc::getgroups(0, std::ptr::null_mut()) } == 0,
            "incomplete sandbox identity drop"
        );
        Ok(())
    }
    pub(crate) fn validate_workspace(self, workspace: &Path) -> Result<()> {
        self.validate()?;
        ensure!(
            self.uid != unsafe { libc::geteuid() },
            "agent must differ from runner"
        );
        for (index, path) in workspace.ancestors().enumerate() {
            let meta = std::fs::metadata(path)?;
            let bits = if meta.uid() == self.uid {
                (meta.mode() >> 6) & 7
            } else if meta.gid() == self.gid {
                (meta.mode() >> 3) & 7
            } else {
                meta.mode() & 7
            };
            ensure!(
                meta.is_dir() && bits & 1 == 1 && (index != 0 || bits & 6 == 6),
                "workspace inaccessible to agent; keep private runner state separate"
            );
        }
        Ok(())
    }
}
/// Bootstrap before creating runtime threads. The backend must already permit
/// SETUID/SETGID/KILL; this never changes its launch policy. Helper and tmux
/// execs inherit only those three capabilities, which the pane clears entirely.
pub fn bootstrap_runner(runner: AgentIdentity, agent: AgentIdentity) -> Result<()> {
    runner.validate()?;
    agent.validate()?;
    single_threaded()?;
    ensure!(
        runner.uid != agent.uid && runner.gid != agent.gid,
        "distinct runner/agent identities required"
    );
    let current = caps()?;
    ensure!(
        current[0].permitted & RUNNER_CAPS == RUNNER_CAPS,
        "sandbox requires SETUID/SETGID/KILL capabilities"
    );
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "runner bootstrap requires root entrypoint"
    );
    syscall_result(unsafe { libc::setgroups(0, std::ptr::null()) } == 0)?;
    syscall_result(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) } == 0)?;
    syscall_result(unsafe { libc::setgid(runner.gid) } == 0)?;
    syscall_result(unsafe { libc::setuid(runner.uid) } == 0)?;
    set_caps(&mut [
        Caps {
            effective: RUNNER_CAPS,
            permitted: RUNNER_CAPS,
            inheritable: RUNNER_CAPS,
        },
        Caps::default(),
    ])?;
    for capability in [5, 6, 7] {
        syscall_result(
            unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_RAISE,
                    capability,
                    0,
                    0,
                )
            } == 0,
        )?;
    }
    syscall_result(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) } == 0)?;
    Ok(())
}

fn named_identity(name: &std::ffi::CStr) -> Result<AgentIdentity> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
    let code = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    ensure!(
        code == 0 && !found.is_null(),
        "required sandbox OS account unavailable"
    );
    let entry = unsafe { entry.assume_init() };
    let identity = AgentIdentity {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    };
    identity.validate()?;
    Ok(identity)
}
/// Production image account lookup and bootstrap, before creating any threads.
/// The returned identity is sealed by `SessionHost::launch_as_agent`.
pub fn bootstrap_sandbox_users() -> Result<AgentIdentity> {
    single_threaded()?;
    let runner = named_identity(c"runner")?;
    let agent = named_identity(c"agent")?;
    bootstrap_runner(runner, agent)?;
    Ok(agent)
}

/// Public startup bytes only, separate from the private host state. The root-
/// owned sticky parent prevents the agent from replacing the directory during
/// publication. Exclusive creation makes a precreated name a refusal.
pub(crate) fn context_directory(launch_id: uuid::Uuid) -> std::path::PathBuf {
    Path::new("/tmp").join(format!("aq-context-{}-{launch_id}", unsafe {
        libc::geteuid()
    }))
}
pub(crate) fn publish_context(launch_id: uuid::Uuid, context: &[Vec<u8>]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let parent = std::fs::symlink_metadata("/tmp")?;
    ensure!(
        parent.is_dir()
            && parent.uid() == 0
            && parent.mode() & 0o005 == 0o005
            && (parent.mode() & 0o022 == 0 || parent.mode() & 0o1000 != 0),
        "startup delivery requires root-owned protected /tmp"
    );
    let directory = context_directory(launch_id);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    for (index, bytes) in context.iter().enumerate() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(format!("context-{index}.bin")))?;
        file.write_all(bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
        file.sync_all()?;
    }
    let dir = std::fs::File::open(&directory)?;
    dir.set_permissions(std::fs::Permissions::from_mode(0o555))?;
    dir.sync_all()?;
    std::fs::File::open("/tmp")?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_identities_refuse_root_and_missing_accounts() {
        for identity in [
            AgentIdentity { uid: 0, gid: 1 },
            AgentIdentity { uid: 1, gid: 0 },
            AgentIdentity {
                uid: u32::MAX,
                gid: 1,
            },
        ] {
            assert!(identity.validate().is_err());
            assert!(identity.drop_privileges().is_err());
        }
        assert!(named_identity(c"aq-b6b-nonexistent-user").is_err());
    }
    #[test]
    fn agent_workspace_refuses_runner_private_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let agent = AgentIdentity {
            uid: unsafe { libc::geteuid() }.wrapping_add(100),
            gid: unsafe { libc::getegid() }.wrapping_add(100),
        };
        assert!(agent.validate_workspace(&workspace).is_err());
    }
}
