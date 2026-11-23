mod lifecycle;
use std::sync::atomic::{AtomicBool, Ordering};
static IN_GUARDIAN: AtomicBool = AtomicBool::new(false);
pub(crate) fn in_guardian() -> bool {
    IN_GUARDIAN.load(Ordering::Relaxed)
}
use super::*;
use ownership::{
    EmergencyReaper, process_evidence, raw_boot_ms, reap_children, signal_children_bounded,
    verified_pidfd,
};
#[cfg(feature = "test-support")]
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
pub(crate) fn machine() -> Result<(String, String)> {
    Ok((
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into(),
        std::fs::read_link("/proc/self/ns/pid")?
            .to_string_lossy()
            .into_owned(),
    ))
}
pub(crate) fn guardian_live(id: &GuardianIdentity) -> bool {
    machine().is_ok_and(|(boot, ns)| boot == id.machine_boot_id && ns == id.pid_namespace)
        && verified_pidfd(&id.process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false))
}
fn save(dir: &Path, state: &HostObservation) -> Result<()> {
    durable_replace(&dir.join("state.json"), &serde_json::to_vec(state)?)
}
fn request(dir: &Path, name: &str, id: &GuardianIdentity) -> Result<Option<ExecutionPermit>> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let request: BoundRequest = read_json(&path)?;
    ensure!(
        request.guardian == *id,
        "request guardian identity mismatch"
    );
    Ok(Some(request.permit))
}
fn parent(pid: u32) -> Result<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("process parent stat")?
        .1
        .split_whitespace()
        .nth(1)
        .context("process parent")?
        .parse()?)
}
fn descendant(mut pid: u32, ancestor: u32) -> Result<bool> {
    for _ in 0..128 {
        if pid == ancestor {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        pid = parent(pid)?;
    }
    Ok(false)
}
fn pane_status(config: &SealedLaunch, dir: &Path) -> Result<(u32, u32, bool, String)> {
    let bytes = tmux(
        config,
        dir,
        &[
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{pid} #{pane_pid} #{pane_dead} #{pane_id}",
        ],
    )?;
    let text = String::from_utf8(bytes)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 4, "tmux identity response");
    Ok((
        fields[0].parse()?,
        fields[1].parse()?,
        fields[2] == "1",
        fields[3].into(),
    ))
}
pub(crate) fn run(dir: &Path) -> Result<()> {
    ownership::become_subreaper()?;
    IN_GUARDIAN.store(true, Ordering::Relaxed);
    let directory_guard = terminal_io::DirectoryGuard::open(dir)?;
    directory_guard.anchor()?;
    let (config, intent) = read_launch(dir)?;
    let identity = claim_guardian(dir, intent)?;
    let state = waiting(&identity, &config);
    let mut observed_owner = if config.observed.is_some() {
        Some(observed::gate::Owner::new(dir)?)
    } else {
        None
    };
    save(dir, &state)?;
    durable_new(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
    let _reaper = EmergencyReaper;
    let reader_owner = reader_lease::Owner::new(dir)?;
    let viewer_owner = reader_lease::Owner::new_viewer(dir)?;
    lifecycle::Guardian {
        dir,
        config: &config,
        identity: &identity,
        directory_guard: &directory_guard,
        state,
        observed_owner: &mut observed_owner,
        reader_owner,
        viewer_owner,
        viewer_revision_highwater: 0,
        _root: None,
        root_pid: None,
        stop_started: None,
        pane_fd: None,
        terminal_failed: false,
        terminal_revision_highwater: 0,
        last_time: raw_boot_ms(),
        wall_deadline: config.wall_deadline()?,
    }
    .supervise()
}
fn read_launch(dir: &Path) -> Result<(SealedLaunch, LaunchIntent)> {
    let config: SealedLaunch = read_json(&dir.join("launch.json"))?;
    config.manifest.validate()?;
    config.command.validate()?;
    if let Some(spec) = &config.observed {
        spec.validate(&config)?;
    }
    let intent = config.intent()?;
    config.validate_permit_at(
        &config.permit,
        &intent.session,
        &intent.manifest_digest,
        raw_boot_ms(),
    )?;
    Ok((config, intent))
}

fn claim_guardian(dir: &Path, intent: LaunchIntent) -> Result<GuardianIdentity> {
    durable_new(
        &dir.join("guardian.claim"),
        b"one guardian per immutable launch\n",
    )?;
    let (boot, ns) = machine()?;
    Ok(GuardianIdentity {
        intent,
        nonce: Uuid::new_v4(),
        process: process_evidence(std::process::id())?,
        machine_boot_id: boot,
        pid_namespace: ns,
    })
}

fn waiting(identity: &GuardianIdentity, config: &SealedLaunch) -> HostObservation {
    HostObservation {
        guardian: identity.clone(),
        phase: HostPhase::Waiting,
        pane: None,
        permit_sequence: config.permit.sequence,
        permit_deadline_boot_ms: config.permit.expires_boot_ms,
        observed_boot_ms: raw_boot_ms(),
        reaped_count: 0,
        kernel_wait_echild: false,
        root_status: None,
        stop_reason: None,
    }
}

/// Holds the harness until the guardian has recorded this pane's identity, so a
/// harness that finishes at once never leaves only a dead pane to verify.
fn wait_recorded(
    dir: &Path,
    guardian: &GuardianIdentity,
    server: &ProcessEvidence,
    expires_boot_ms: u64,
) -> Result<()> {
    let me = process_evidence(std::process::id())?;
    loop {
        let state: HostObservation = read_json(&dir.join("state.json"))?;
        ensure!(state.guardian == *guardian, "pane guardian state mismatch");
        if let Some(pane) = &state.pane {
            ensure!(
                pane.pane == me && pane.server == *server,
                "pane identity recorded for another process"
            );
            return Ok(());
        }
        ensure!(
            state.phase == HostPhase::Starting
                && raw_boot_ms() < expires_boot_ms
                && !dir.join("stop.request").exists(),
            "pane cancelled before its identity was recorded"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(crate) fn exec_pane(dir: &Path) -> Result<()> {
    let config: SealedLaunch = read_json(&dir.join("launch.json"))?;
    let identity: GuardianIdentity = read_json(&dir.join("identity.json"))?;
    ensure!(
        config.intent()? == identity.intent && guardian_live(&identity),
        "pane launch identity/guardian mismatch"
    );
    let server: ProcessEvidence = read_json(&dir.join("server.json"))?;
    let _server_live = verified_pidfd(&server)?;
    ensure!(
        descendant(std::process::id(), server.pid)?,
        "pane outside owned server"
    );
    let permit = request(dir, "start.request", &identity)?.context("pane start authorization")?;
    config.validate_permit_at(
        &permit,
        &identity.intent.session,
        &identity.intent.manifest_digest,
        raw_boot_ms(),
    )?;
    ensure!(
        !dir.join("stop.request").exists() && raw_boot_ms() < config.wall_deadline()?,
        "pane cancelled/wall limit expired"
    );
    config.manifest.verify_startup(dir)?;
    if let Some(rendered) = &config.rendered_startup {
        let bytes = read_bytes(&dir.join("context-0.bin"), 1_048_576)?;
        rendered.verify(config.manifest.task()?, &bytes)?;
    }
    if config.observed.is_some() {
        return observed::gate::wait_closed(dir, &config, &identity, &server);
    }
    wait_recorded(dir, &identity, &server, permit.expires_boot_ms)?;
    durable_new(
        &dir.join("pane-exec.claim"),
        b"one native harness exec; no fallback shell\n",
    )?;
    let mut command = Command::new(&config.command.executable);
    command
        .args(&config.command.arguments)
        .current_dir(&config.workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("AQ_SESSION_DIRECTORY", dir)
        .envs(&config.command.environment);
    config.validate_permit_at(
        &permit,
        &identity.intent.session,
        &identity.intent.manifest_digest,
        raw_boot_ms(),
    )?;
    ensure!(
        raw_boot_ms() < config.wall_deadline()? && !dir.join("stop.request").exists(),
        "authority expired/cancelled immediately before native exec"
    );
    if let Some(agent) = config.agent_identity {
        command.env(
            "AQ_SESSION_DIRECTORY",
            crate::users::context_directory(config.launch_id),
        );
        // All runner-private inputs were read before this irreversible change.
        // Chdir first so no private directory remains as the agent's cwd.
        std::env::set_current_dir(&config.workspace)?;
        agent.drop_privileges()?;
    }
    let error = command.exec();
    Err(error).context("native harness exec failed; no fallback")
}
/// Deterministic interactive fixture, explicitly not a real harness adapter.
#[cfg(feature = "test-support")]
pub(crate) fn fixture(dir: &Path, mode: &str) -> Result<()> {
    durable_new(&dir.join("fixture.claim"), b"synthetic interactive child\n")?;
    durable_new(
        &dir.join("fixture-process.json"),
        &serde_json::to_vec(&process_evidence(std::process::id())?)?,
    )?;
    if mode == "exit" {
        return Ok(());
    }
    if mode == "rendered" {
        let bytes = read_bytes(&dir.join("context-0.bin"), 1_048_576)?;
        durable_new(&dir.join("rendered-received.bin"), &bytes)?;
    }
    if mode == "fork-flood" {
        let producer = unsafe { libc::fork() };
        ensure!(producer >= 0, "producer fork");
        if producer == 0 {
            unsafe {
                libc::setsid();
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let result = (|| -> Result<()> {
                durable_new(
                    &dir.join("producer-process.json"),
                    &serde_json::to_vec(&process_evidence(std::process::id())?)?,
                )?;
                let mut count = 0u64;
                loop {
                    let first = unsafe { libc::fork() };
                    if first == 0 {
                        let second = unsafe { libc::fork() };
                        unsafe { libc::_exit(if second < 0 { 92 } else { 0 }) }
                    }
                    ensure!(first > 0, "flood fork");
                    let mut status = 0;
                    unsafe {
                        libc::waitpid(first, &mut status, 0);
                    };
                    count += 1;
                    if count.is_multiple_of(16) {
                        durable_replace(
                            &dir.join("producer-counter"),
                            count.to_string().as_bytes(),
                        )?;
                    }
                }
            })();
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 93 }) }
        }
    }
    if mode == "escaped" {
        let first = unsafe { libc::fork() };
        ensure!(first >= 0, "fixture fork");
        if first == 0 {
            if unsafe { libc::setsid() } < 0 {
                unsafe { libc::_exit(61) }
            }
            let second = unsafe { libc::fork() };
            if second < 0 {
                unsafe { libc::_exit(62) }
            }
            if second > 0 {
                unsafe { libc::_exit(0) }
            }
            let result = (|| -> Result<()> {
                durable_new(
                    &dir.join("escaped-process.json"),
                    &serde_json::to_vec(&process_evidence(std::process::id())?)?,
                )?;
                let mut count = 0u64;
                loop {
                    count += 1;
                    durable_replace(&dir.join("escaped-counter"), count.to_string().as_bytes())?;
                    std::thread::sleep(Duration::from_millis(5));
                }
            })();
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 63 }) }
        }
        let mut status = 0;
        ensure!(
            unsafe { libc::waitpid(first, &mut status, 0) } == first,
            "fixture intermediate reap"
        );
    }
    fixture_input(dir, mode)
}

#[cfg(feature = "test-support")]
fn fixture_input(dir: &Path, mode: &str) -> Result<()> {
    println!("AQ_FIXTURE_READY");
    std::io::stdout().flush()?;
    for line in BufReader::new(std::io::stdin()).lines() {
        let line = line?;
        match (mode, line.as_str()) {
            ("output-pattern", "emit") => emit_pattern()?,
            ("output-flood", "flood") => emit_flood(dir)?,
            ("terminal-journal", _) => journal_line(dir, &line)?,
            _ => {}
        }
        if line == "exit" {
            break;
        }
        println!("echo:{line}");
        std::io::stdout().flush()?;
    }
    Ok(())
}

#[cfg(feature = "test-support")]
fn emit_pattern() -> Result<()> {
    std::io::stdout()
        .write_all("\x1b[?1049h\x1b[2J\x1b[3;4H雪🙂e\u{301}\x1b[?1049l\n".as_bytes())?;
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(feature = "test-support")]
fn emit_flood(dir: &Path) -> Result<()> {
    std::fs::write(
        dir.join("fixture-output-start"),
        b"synthetic flood accepted",
    )?;
    let block = vec![b'x'; 8192];
    for n in 1..=4096u64 {
        std::io::stdout().write_all(&block)?;
        if n.is_multiple_of(8) {
            std::io::stdout().flush()?;
            // Volatile synthetic progress: fsync latency must not stand in
            // for PTY backpressure in the stalled-reader qualification.
            std::fs::write(
                dir.join("fixture-output-progress"),
                (n * block.len() as u64).to_string().as_bytes(),
            )?;
        }
    }
    Ok(())
}

#[cfg(feature = "test-support")]
fn journal_line(dir: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("fixture-input.jsonl"))?;
    let mut encoded = serde_json::to_vec(&line)?;
    encoded.push(b'\n');
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn fault_point(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    if std::env::var("AQ_SESSION_HOST_FAULT_STAGE").ok().as_deref() == Some(_stage) {
        durable_new(&_dir.join("guardian-fault.json"), _stage.as_bytes())?;
        unsafe { libc::raise(libc::SIGSTOP) };
    }
    Ok(())
}
