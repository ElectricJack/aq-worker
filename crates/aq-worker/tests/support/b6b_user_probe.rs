//! Numeric UID kernel fixture, independent of sandbox image users.
use anyhow::{Result, ensure};
use aq_session_host::{
    users::{AgentIdentity, bootstrap_sandbox_users},
    *,
};
use aq_worker::native::runner::sandbox::private::RunnerFiles;
use std::{
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    time::Duration,
};
const RUNNER: AgentIdentity = AgentIdentity {
    uid: 21001,
    gid: 21001,
};
const AGENT: AgentIdentity = AgentIdentity {
    uid: 21002,
    gid: 21002,
};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if matches!(args.get(1).map(String::as_str), Some("guardian" | "pane")) {
        return helper_main();
    }
    if args.get(1).is_some_and(|s| s == "setuid-positive-control") {
        ensure!(
            unsafe { libc::getuid() } == AGENT.uid && unsafe { libc::geteuid() } == 0,
            "fixture setuid executable did not elevate without NNP"
        );
        ensure!(
            fs::read_to_string("/proc/self/status")?
                .lines()
                .any(|line| line == "CapEff:\t00000000000000e0"),
            "fixture setuid executable did not restore allowed capabilities"
        );
        println!("positive control: setuid exec without NNP restored root and CapEff=0xe0");
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "exec-privilege-probe") {
        agent_privileges()?;
        println!(
            "setuid exec with NNP retained agent UID, empty Inh/Prm/Eff/Amb and bounded CapBnd"
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "agent-probe") {
        // Like a tmux pane, the standalone probe must have its own session:
        // Linux permits cross-UID SIGCONT within a shared session.
        ensure!(unsafe { libc::setsid() } >= 0, "fixture agent session");
        AGENT.drop_privileges()?;
        agent_probe(Path::new(&args[2]), args[3].parse()?)?;
        checked_output(
            std::process::Command::new(&args[4])
                .arg("exec-privilege-probe")
                .env_clear(),
        )?;
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "harness") {
        let runner_pid = args[3].parse()?;
        agent_probe(Path::new(&args[2]), runner_pid)?;
        let input = std::env::var_os("AQ_SESSION_DIRECTORY")
            .ok_or_else(|| anyhow::anyhow!("missing startup context directory"))?;
        ensure!(
            fs::read(Path::new(&input).join("context-0.bin"))? == b"fixture",
            "required startup context unavailable"
        );

        // Escaped writer exercises guardian stop across UID transition.
        let child = unsafe { libc::fork() };
        ensure!(child >= 0, "fixture fork");
        if child == 0 {
            unsafe {
                libc::setsid();
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            loop {
                fs::write("counter", format!("{:?}", std::time::Instant::now()))?;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fs::write("started", b"agent started")?;
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Fixture-local accounts exercise the same name lookup as sandbox-run.
    let mut passwd = fs::read_to_string("/etc/passwd")?;
    passwd.push_str(
        "runner:x:21001:21001:runner:/tmp:/bin/sh\nagent:x:21002:21002:agent:/tmp:/bin/sh\n",
    );
    fs::write("/etc/passwd", passwd)?;
    // A disposable fixture trap, never a production helper. The positive
    // control proves this filesystem honors setuid before testing the NNP fence.
    let exec_dir = tempfile::tempdir()?;
    fs::set_permissions(exec_dir.path(), fs::Permissions::from_mode(0o755))?;
    let setuid_executable = exec_dir.path().join("setuid-probe");
    fs::copy(std::env::current_exe()?, &setuid_executable)?;
    fs::set_permissions(&setuid_executable, fs::Permissions::from_mode(0o4755))?;
    let meta = fs::metadata(&setuid_executable)?;
    ensure!(
        meta.uid() == 0 && meta.mode() & 0o7777 == 0o4755,
        "setuid fixture metadata"
    );
    let mut control = std::process::Command::new(&setuid_executable);
    control.arg("setuid-positive-control").env_clear();
    unsafe {
        control.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(AGENT.gid) != 0
                || libc::setuid(AGENT.uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    checked_output(&mut control)?;
    ensure!(bootstrap_sandbox_users()? == AGENT, "named agent identity");
    ensure!(
        unsafe { libc::getuid() } == RUNNER.uid
            && unsafe { libc::getgid() } == RUNNER.gid
            && unsafe { libc::getgroups(0, std::ptr::null_mut()) } == 0,
        "runner identity/groups"
    );
    let dir = tempfile::tempdir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o711))?;
    let files = RunnerFiles::create(&dir.path().join("runner"))?;
    for (name, body) in [
        ("permit.json", b"permit-secret".as_slice()),
        ("provider.key", b"provider-secret"),
        ("access.json", b"access-secret"),
    ] {
        files.write_secret(name, body)?;
    }
    durable_new(&files.root().join("state/journal.json"), b"journal-secret")?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "agent-probe",
            files.root().to_str().unwrap(),
            &std::process::id().to_string(),
            setuid_executable.to_str().unwrap(),
        ])
        .env_clear()
        .output()?;
    ensure!(
        output.status.success(),
        "agent probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace)?;
    // Fixture-only shared workspace; production hydration supplies agent ownership.
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o777))?;
    let mut manifest = serde_json::from_slice::<aq_session_protocol::LaunchManifest>(
        include_bytes!("../../../aq-session-host/tests/fixtures/manifest.json"),
    )?;
    manifest.terminal.input_enabled = false;
    let context = files.root().join("context");
    durable_new(&context, b"fixture")?;
    let local = LocalEnvironment {
        resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.adapter.clone()),
        workspace: workspace.clone(),
        helper_executable: std::env::current_exe()?,
        tmux_executable: "/usr/bin/tmux".into(),
        tmux_version: String::from_utf8(
            std::process::Command::new("/usr/bin/tmux")
                .arg("-V")
                .output()?
                .stdout,
        )?
        .trim()
        .into(),
        credential_profiles: vec![manifest.harness.adapter.clone()],
        context_material: vec![ContextMaterial {
            artifact: manifest.harness.adapter.clone(),
            path: context,
        }],
    };
    let now = boot_ms();
    let permit = ExecutionPermit {
        session: manifest.bindings.session.clone(),
        manifest_digest: aq_session_protocol::canonical_digest(&manifest)?,
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 5000,
    };
    let mut handle = SessionHost::launch_as_agent(
        &files.root().join("state/hosts"),
        uuid::Uuid::new_v4(),
        &manifest,
        permit,
        &local,
        &Adapter(files.root().to_owned(), std::process::id()),
        &mut Authority,
        AGENT,
    )?;
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while !workspace.join("started").exists() || !workspace.join("counter").exists() {
        ensure!(
            std::time::Instant::now() < end,
            "separated harness did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.stop()?;
    handle.wait_stopped(Duration::from_secs(5))?;
    handle.stopped_proof()?;
    let before = fs::read(workspace.join("counter"))?;
    std::thread::sleep(Duration::from_millis(50));
    ensure!(
        before == fs::read(workspace.join("counter"))?,
        "agent descendant still writing"
    );
    println!(
        "runner UID 21001, agent UID 21002; credential denial, empty groups and Inh/Prm/Eff/Amb, residual CapBnd=0xe0, no_new_privs=1, outside-pane signal 0/SIGCONT denied with EPERM, tmux execution and descendant stop proven"
    );
    Ok(())
}
fn boot_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) },
        0
    );
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}
fn agent_probe(root: &Path, runner_pid: libc::pid_t) -> Result<()> {
    agent_privileges()?;
    signal_denial(runner_pid)?;
    for name in [
        "permit.json",
        "provider.key",
        "access.json",
        "state/journal.json",
    ] {
        ensure!(
            fs::read(root.join(name)).unwrap_err().kind() == std::io::ErrorKind::PermissionDenied,
            "agent read runner material"
        );
    }
    for value in std::env::vars_os()
        .map(|(_, v)| v)
        .chain(std::env::args_os())
    {
        ensure!(
            !value.to_string_lossy().contains("provider-secret")
                && !value.to_string_lossy().contains("access-secret"),
            "agent secret exposure"
        );
    }
    ensure!(
        RUNNER.drop_privileges().is_err(),
        "valid transition without capabilities must refuse"
    );
    ensure!(
        unsafe { libc::getuid() } == AGENT.uid,
        "failed transition changed agent UID"
    );
    ensure!(unsafe { libc::setuid(0) } != 0, "agent regained root");
    Ok(())
}
fn checked_output(command: &mut std::process::Command) -> Result<()> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "exec probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
fn signal_denial(runner_pid: libc::pid_t) -> Result<()> {
    // Owned by the prerequisite worker; retain its single outside-pane probe.
    let runner_status = fs::read_to_string(format!("/proc/{runner_pid}/status"))?;
    ensure!(
        runner_pid > 0
            && runner_pid != unsafe { libc::getpid() }
            && runner_status
                .lines()
                .any(|line| { line == format!("Uid:\t{0}\t{0}\t{0}\t{0}", RUNNER.uid) }),
        "signal target must be the live runner process"
    );
    let agent_session = unsafe { libc::getsid(0) };
    let runner_session = unsafe { libc::getsid(runner_pid) };
    ensure!(
        agent_session > 0 && runner_session > 0 && agent_session != runner_session,
        "signal target must be outside the agent pane session"
    );
    for signal in [0, libc::SIGCONT] {
        ensure!(
            unsafe { libc::kill(runner_pid, signal) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
            "agent could signal runner outside its pane"
        );
    }
    Ok(())
}
fn agent_privileges() -> Result<()> {
    ensure!(
        unsafe { libc::getuid() } == 21002
            && unsafe { libc::geteuid() } == 21002
            && unsafe { libc::getgid() } == 21002
            && unsafe { libc::getegid() } == 21002,
        "agent IDs"
    );
    ensure!(
        unsafe { libc::getgroups(0, std::ptr::null_mut()) } == 0,
        "agent supplementary groups"
    );
    let status = fs::read_to_string("/proc/self/status")?;
    ensure!(
        status.lines().any(|line| line == "NoNewPrivs:\t1"),
        "agent may regain exec privileges"
    );
    for field in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
        ensure!(
            status
                .lines()
                .any(|line| line == format!("{field}:\t0000000000000000")),
            "agent retained {field} capabilities"
        );
    }
    // The fixture grants exactly KILL/SETGID/SETUID. Clearing their bounding
    // bits would require SETPCAP, which the approved bootstrap does not grant.
    let bound = status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:\t"))
        .ok_or_else(|| anyhow::anyhow!("missing CapBnd"))?;
    ensure!(
        u64::from_str_radix(bound, 16)? & !0xe0 == 0,
        "agent bounding set exceeds KILL/SETGID/SETUID"
    );
    Ok(())
}
struct Adapter(std::path::PathBuf, u32);
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &aq_session_protocol::LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: aq_session_protocol::canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: LocalCommand {
                executable: std::env::current_exe()?,
                arguments: vec![
                    "harness".into(),
                    self.0.to_str().unwrap().into(),
                    self.1.to_string(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Ready)
    }
}
struct Authority;
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, _: &LaunchIntent) -> Result<()> {
        Ok(())
    }
    fn record_guardian(&mut self, _: &GuardianIdentity) -> Result<()> {
        Ok(())
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        Ok(())
    }
    fn record_started(&mut self, _: &HostObservation) -> Result<()> {
        Ok(())
    }
}
