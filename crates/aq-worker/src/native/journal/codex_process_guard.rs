//! Kernel/filesystem premises for explicitly opted-in real CLI qualifications.
//! An environment flag alone never establishes these premises.
use std::{ffi::CString, fs, path::Path};
#[cfg(test)]
pub(super) fn require_confined_fixture() -> anyhow::Result<()> {
    require_confined_binary(Path::new("/opt/aq-qualification/aq-worker-tests"))
}
pub(super) fn require_confined_binary(expected_binary: &Path) -> anyhow::Result<()> {
    let refusal =
        "actual Codex qualification requires scripts/qualify-native-loopback.sh confinement";
    anyhow::ensure!(
        std::env::var("AQ_NATIVE_FIXTURE_CONFINED").as_deref() == Ok("v1"),
        "{refusal}"
    );
    let pid_namespace = fs::read_link("/proc/thread-self/ns/pid")?;
    let outer_namespace = std::env::var("AQ_NATIVE_OUTER_PIDNS")?;
    anyhow::ensure!(
        pid_namespace != Path::new(&outer_namespace),
        "{refusal}: host PID namespace is shared"
    );
    anyhow::ensure!(
        fs::read_link("/proc/1/ns/pid")? == pid_namespace,
        "{refusal}: proc namespace differs"
    );
    let outer = std::env::var("AQ_NATIVE_OUTER_PID")?.parse::<u32>()?;
    anyhow::ensure!(
        !Path::new(&format!("/proc/{outer}/root")).exists()
            && !Path::new(&format!("/proc/{outer}/fd")).exists(),
        "{refusal}: outside process aliases visible"
    );
    let expected = [
        "group",
        "hosts",
        "localtime",
        "mtab",
        "nsswitch.conf",
        "passwd",
        "timezone",
    ];
    for directory in ["/etc", "/proc/1/root/etc"] {
        let mut actual: Vec<_> = fs::read_dir(directory)?
            .map(|p| p.map(|p| p.file_name()))
            .collect::<Result<_, _>>()?;
        actual.sort();
        anyhow::ensure!(
            actual
                .iter()
                .map(|s| s.to_str().unwrap_or(""))
                .collect::<Vec<_>>()
                == expected,
            "{refusal}: operator etc image visible"
        );
    }
    for directory in ["/proc/1/root/home", "/proc/1/root/root"] {
        anyhow::ensure!(
            fs::read_dir(directory)?.next().is_none(),
            "{refusal}: proc root exposes operator homes"
        );
    }
    let map = fs::read_to_string("/proc/thread-self/uid_map")?;
    let rows: Vec<Vec<u64>> = map
        .lines()
        .map(|l| {
            l.split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
        })
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(
        rows.len() == 1 && rows[0].len() == 3 && rows[0][2] == 1,
        "{refusal}: dedicated user namespace absent"
    );
    let devices = fs::read_to_string("/proc/net/dev")?;
    let names: Vec<_> = devices
        .lines()
        .filter_map(|l| l.split_once(':').map(|p| p.0.trim()))
        .collect();
    anyhow::ensure!(names == ["lo"], "{refusal}: network is not loopback-only");
    let routes = fs::read_to_string("/proc/net/route")?;
    anyhow::ensure!(
        routes
            .lines()
            .skip(1)
            .all(|l| l.split_whitespace().nth(1) != Some("00000000")),
        "{refusal}: default route exists"
    );
    for home in ["/home", "/root"] {
        anyhow::ensure!(
            fs::read_dir(home)?.next().is_none(),
            "{refusal}: operator home is visible"
        );
    }
    let state = Path::new("/var/lib/aq-qualification");
    let members: Vec<_> = fs::read_dir("/var/lib")?.collect::<Result<_, _>>()?;
    anyhow::ensure!(
        members.len() == 1 && members[0].path() == state,
        "{refusal}: host service state is visible"
    );
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(state)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.mode() & 0o7777 == 0o700
            && metadata.uid() == unsafe { libc::geteuid() }
            && std::env::var_os("TMPDIR").as_deref() == Some(state.as_os_str()),
        "{refusal}: private state identity differs"
    );
    for (name, expected) in [
        ("AQ_NATIVE_CODEX_BINARY", "/opt/aq-qualification/codex"),
        (
            "AQ_NATIVE_HOST_BINARY",
            "/opt/aq-qualification/aq-session-host",
        ),
        ("AQ_NATIVE_TMUX_BINARY", "/opt/aq-qualification/tmux"),
    ] {
        anyhow::ensure!(
            std::env::var(name).as_deref() == Ok(expected),
            "{refusal}: explicit fixture image path differs"
        );
        readonly_mount(expected, refusal)?;
    }
    anyhow::ensure!(
        std::env::current_exe()? == expected_binary,
        "{refusal}: qualification binary path differs"
    );
    readonly_mount(
        expected_binary
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{refusal}: non-UTF8 image path"))?,
        refusal,
    )?;
    Ok(())
}
fn readonly_mount(path: &str, refusal: &str) -> anyhow::Result<()> {
    let path = CString::new(path)?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    anyhow::ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "{refusal}: image mount unavailable"
    );
    let stat = unsafe { stat.assume_init() };
    anyhow::ensure!(
        stat.f_flag & libc::ST_RDONLY != 0,
        "{refusal}: image mount is writable"
    );
    Ok(())
}

#[test]
#[ignore = "kernel premise probe only; run using the qualification wrapper; never launches Codex"]
fn kernel_fixture_confinement_is_independently_proven() {
    require_confined_fixture().unwrap();
}
