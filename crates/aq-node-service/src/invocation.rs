//! Ruling B (D2), proof (b): the service runs under systemd and its unit's cgroup holds no
//! process of the previous invocation. systemd starts a unit again only after its cgroup was
//! emptied (`KillMode=mixed` sends the remaining processes SIGKILL), and the check confirms it:
//! every process in the cgroup is this process or one of its descendants.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The unit cgroup's `cgroup.procs` of this process (cgroup v2: the `0::` line of
/// `/proc/self/cgroup`), under `cgroup_root` (`/sys/fs/cgroup`).
pub fn own_cgroup_procs(proc_self_cgroup: &str, cgroup_root: &Path) -> Option<PathBuf> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    Some(
        cgroup_root
            .join(path.trim().trim_start_matches('/'))
            .join("cgroup.procs"),
    )
}
/// True when every pid in `procs` is `own` or descends from it (`parent` gives a pid's parent;
/// None ends the walk: the process is gone or unreadable, so it is not ours).
pub fn holds_only(procs: &str, own: u32, parent: impl Fn(u32) -> Option<u32>) -> bool {
    procs.split_whitespace().all(|pid| {
        let Ok(mut pid) = pid.parse::<u32>() else {
            return false;
        };
        for _ in 0..4096 {
            if pid == own {
                return true;
            }
            match parent(pid) {
                Some(p) if p > 0 && p != pid => pid = p,
                _ => return false,
            }
        }
        false
    })
}
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
/// The unit a cgroup.procs path belongs to: its directory's name, when it is a `.service`.
pub fn unit_of(procs: &Path) -> Option<String> {
    let name = procs.parent()?.file_name()?.to_str()?;
    name.ends_with(".service").then(|| name.to_owned())
}
/// Proof (b): under systemd, the unit's cgroup holds only this process and its descendants.
/// Some((`INVOCATION_ID`, unit name)) then; None when not under systemd or any other process
/// is there.
///
/// Scope (#51 review M1): `INVOCATION_ID` is read from this process's own environment. The
/// children that matter here are started with a cleared environment (the session host and its
/// driver, `env_clear`), so they neither see nor pass it on; the check relies only on systemd
/// keeping every descendant in the unit's cgroup (no `Delegate=`, no scope escape), which is
/// what makes a process outside our descendants a survivor of an earlier invocation. A child
/// that moved itself to another cgroup would not be seen: no AQ child does.
pub fn prior_invocation_gone() -> Result<Option<(String, String)>> {
    let Some(invocation) = std::env::var("INVOCATION_ID")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let own = std::fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
    let procs = own_cgroup_procs(&own, Path::new("/sys/fs/cgroup"))
        .context("no cgroup v2 entry in /proc/self/cgroup")?;
    let unit = unit_of(&procs).context("the service's cgroup is not a .service unit")?;
    let listed =
        std::fs::read_to_string(&procs).with_context(|| format!("read {}", procs.display()))?;
    Ok(holds_only(&listed, std::process::id(), parent_of).then_some((invocation, unit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_the_units_cgroup_procs() {
        let cgroup =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/aq-node@ms1.service\n";
        assert_eq!(
            own_cgroup_procs(cgroup, Path::new("/sys/fs/cgroup")).unwrap(),
            Path::new(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/aq-node@ms1.service/cgroup.procs"
            )
        );
        assert_eq!(
            own_cgroup_procs("12:cpu:/x\n", Path::new("/sys/fs/cgroup")),
            None
        );
        let procs = own_cgroup_procs(cgroup, Path::new("/sys/fs/cgroup")).unwrap();
        assert_eq!(unit_of(&procs).as_deref(), Some("aq-node@ms1.service"));
        assert_eq!(
            unit_of(Path::new("/sys/fs/cgroup/user.slice/cgroup.procs")),
            None
        );
    }
    /// Only our own pid and our descendants: the previous invocation is gone. A foreign pid
    /// (an old guardian reparented to the user manager) means it is not.
    #[test]
    fn only_this_process_and_its_descendants() {
        let parents = |pid| match pid {
            200 => Some(100), // our aq-git
            201 => Some(200), // its engine
            300 => Some(1),   // reparented survivor of the old invocation
            _ => None,
        };
        assert!(holds_only("100\n", 100, parents));
        assert!(holds_only("100\n200\n201\n", 100, parents));
        assert!(!holds_only("100\n200\n300\n", 100, parents));
        assert!(!holds_only("100\n999\n", 100, parents), "unreadable");
        assert!(!holds_only("100\nnot-a-pid\n", 100, parents));
        // A fake cgroup on disk, read the way the service reads it.
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("app.slice/aq-node@t.service");
        std::fs::create_dir_all(&unit).unwrap();
        let own = std::process::id();
        std::fs::write(unit.join("cgroup.procs"), format!("{own}\n")).unwrap();
        let procs = own_cgroup_procs("0::/app.slice/aq-node@t.service\n", dir.path()).unwrap();
        let listed = std::fs::read_to_string(&procs).unwrap();
        assert!(holds_only(&listed, own, parent_of));
        std::fs::write(&procs, format!("{own}\n1\n")).unwrap();
        assert!(!holds_only(
            &std::fs::read_to_string(&procs).unwrap(),
            own,
            parent_of
        ));
    }
}
