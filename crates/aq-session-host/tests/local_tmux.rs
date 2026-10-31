//! Explicit local process qualification: cargo test -p aq-session-host
//! --features test-support --test local_tmux -- --ignored
use anyhow::Result;
use aq_session_host::*;
use aq_session_protocol::{LaunchManifest, SessionIdentity, canonical_digest};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;
fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "local process predicate timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
struct Adapter {
    mode: String,
    dir: PathBuf,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: LocalCommand {
                executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
                arguments: vec![
                    "fixture".into(),
                    self.dir.to_str().unwrap().into(),
                    self.mode.clone(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, o: &HostObservation) -> Result<HarnessReadiness> {
        Ok(if o.phase == HostPhase::Stopped {
            HarnessReadiness::Exited
        } else if self.dir.join("fixture.claim").exists() {
            HarnessReadiness::Ready
        } else {
            HarnessReadiness::Unknown
        })
    }
}
struct Authority {
    root: PathBuf,
    pause: Option<String>,
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        durable_new(
            &self.root.join("caller-intent.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-guardian.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-start"),
            b"durable start authorization\n",
        )
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        durable_new(
            &self.root.join("caller-started.json"),
            &serde_json::to_vec(o)?,
        )
    }
    fn checkpoint(&mut self, s: &str) -> Result<()> {
        if self.pause.as_deref() == Some(s) {
            durable_new(&self.root.join("caller-checkpoint"), s.as_bytes())?;
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}
struct Case {
    temp: TempDir,
    manifest: LaunchManifest,
    local: LocalEnvironment,
    adapter: Adapter,
    authority: Authority,
    id: Uuid,
}
impl Case {
    fn new(mode: &str, central: bool) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("aqh-")
            .tempdir_in("/tmp")
            .unwrap();
        Self::at(temp, mode, central)
    }
    fn at(temp: TempDir, mode: &str, central: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::from_u128(2);
        let mut manifest: LaunchManifest =
            serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
        if central {
            manifest.bindings.session = SessionIdentity::Central {
                authority_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            };
        }
        let material = temp.path().join("context");
        std::fs::write(&material, b"fixture").unwrap();
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
            workspace: temp.path().canonicalize().unwrap(),
            helper_executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: "tmux 3.4".into(),
            credential_profiles: manifest
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect(),
            context_material: vec![ContextMaterial {
                artifact: manifest.context.references[0].artifact.clone(),
                path: material,
            }],
        };
        let adapter = Adapter {
            mode: mode.into(),
            dir: temp.path().join(id.to_string()),
        };
        let authority = Authority {
            root: temp.path().into(),
            pause: None,
        };
        Self {
            temp,
            manifest,
            local,
            adapter,
            authority,
            id,
        }
    }
    fn launch(&mut self, ms: u64) -> Result<SessionHandle> {
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: self.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&self.manifest)?,
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + ms,
        };
        SessionHost::launch(
            self.temp.path(),
            self.id,
            &self.manifest,
            p,
            &self.local,
            &self.adapter,
            &mut self.authority,
        )
    }
}
fn tmux(h: &SessionHandle, args: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(h.directory().join("tmux.sock"))
        .args(args)
        .output()
        .unwrap()
}
fn terminate(e: &ProcessEvidence) {
    if let Ok(fd) = ownership::verified_pidfd(e) {
        let _ = ownership::signal(&fd, libc::SIGKILL);
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn central_and_remote_interaction_stop_and_no_relaunch() {
    for central in [false, true] {
        let mut c = Case::new("interactive", central);
        let mut h = c.launch(5000).unwrap();
        wait(|| std::fs::read_to_string(h.directory().join("fixture.claim")).ok());
        let o = h.observe().unwrap();
        let pane = o.pane.unwrap();
        assert_eq!(
            pane.server.pid,
            serde_json::from_slice::<ProcessEvidence>(
                &std::fs::read(h.directory().join("server.json")).unwrap()
            )
            .unwrap()
            .pid
        );
        assert!(
            tmux(&h, &["send-keys", "-l", "hello; $(unsafe)"])
                .status
                .success()
        );
        assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
        wait(|| {
            h.capture_text()
                .ok()
                .filter(|s| s.contains("echo:hello; $(unsafe)"))
        });
        h.stop().unwrap();
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        assert_eq!(h.stopped_proof().unwrap().guardian(), h.identity());
        assert!(ownership::process_evidence(pane.server.pid).is_err());
        assert!(h.capture_text().is_err());
        assert!(
            !tmux(&h, &["display-message", "-p", "#{pid}"])
                .status
                .success()
        );
        assert!(c.launch(5000).is_err());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn pane_exit_has_no_fallback_and_escaped_descendants_require_kernel_stop() {
    let mut c = Case::new("escaped", false);
    let mut h = c.launch(5000).unwrap();
    wait(|| std::fs::read(h.directory().join("escaped-process.json")).ok());
    let e: ProcessEvidence =
        serde_json::from_slice(&std::fs::read(h.directory().join("escaped-process.json")).unwrap())
            .unwrap();
    assert!(tmux(&h, &["send-keys", "-l", "exit"]).status.success());
    assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild && s.reaped_count >= 2);
    assert!(ownership::process_evidence(e.pid).is_err());
    assert_eq!(
        std::fs::read(h.directory().join("pane-exec.claim")).unwrap(),
        b"one native harness exec; no fallback shell\n"
    );
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn permit_expiry_is_independent_and_late_renewal_cannot_revive() {
    let mut c = Case::new("interactive", false);
    let mut h = c.launch(500).unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.stop_reason.as_deref().unwrap().contains("expired"));
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 1000,
    };
    assert!(h.renew(&p).is_err());
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_loss_remains_uncertain_even_after_known_processes_disappear() {
    let mut c = Case::new("interactive", true);
    let h = c.launch(5000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let o = h.observe().unwrap();
    terminate(&o.guardian.process);
    wait(|| (h.observe().unwrap().phase == HostPhase::StopUncertain).then_some(()));
    assert!(h.stop().is_err());
    let p = o.pane.unwrap();
    terminate(&p.server);
    terminate(&p.pane);
    assert_eq!(h.observe().unwrap().phase, HostPhase::StopUncertain);
}
#[test]
#[ignore = "subprocess fixture, called by local crash tests only"]
fn launch_child_fixture() {
    let Ok(root) = std::env::var("AQ_HOST_CASE_ROOT") else {
        return;
    };
    let temp = TempDir::new_in(&root).unwrap();
    let mut c = Case::at(temp, "interactive", false);
    std::fs::write(
        Path::new(&root).join("case-root"),
        c.temp.path().to_str().unwrap(),
    )
    .unwrap();
    c.authority.pause = std::env::var("AQ_HOST_CALLER_PAUSE").ok();
    let _handle = c.launch(1500);
    std::thread::sleep(Duration::from_secs(2));
    std::mem::forget(c);
}
fn spawn_case(root: &Path, stage: &str, guardian_fault: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "launch_child_fixture",
            "--test-threads=1",
        ])
        .env("AQ_HOST_CASE_ROOT", root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if guardian_fault {
        command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
    } else {
        command.env("AQ_HOST_CALLER_PAUSE", stage);
    }
    command.spawn().unwrap()
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn caller_crashes_at_durable_start_boundaries_never_relaunch() {
    for stage in [
        "launch-intent-durable",
        "guardian-spawned",
        "guardian-identity-durable",
        "start-intent-durable",
        "start-sent",
        "start-receipt-durable",
    ] {
        let root = TempDir::new().unwrap();
        let mut caller = spawn_case(root.path(), stage, false);
        let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
        let case = Path::new(&case);
        wait(|| case.join("caller-checkpoint").exists().then_some(()));
        caller.kill().unwrap();
        caller.wait().unwrap();
        let intent: LaunchIntent =
            serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap())
                .unwrap();
        if stage == "launch-intent-durable" {
            assert!(
                !case
                    .join(intent.launch_id.to_string())
                    .join("identity.json")
                    .exists()
            );
            continue;
        }
        let mut h = wait(|| SessionHost::recover(case, &intent).ok());
        let s = h
            .wait_stopped(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{stage}: {e:#}"));
        assert!(s.kernel_wait_echild);
        assert!(
            Command::new("/usr/bin/tmux")
                .args(["-N", "-S"])
                .arg(h.directory().join("tmux.sock"))
                .arg("list-sessions")
                .output()
                .is_ok_and(|o| !o.status.success())
        );
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_crashes_across_socket_and_pane_creation_never_claim_stop() {
    for stage in [
        "before-tmux-spawn",
        "after-tmux-spawn",
        "socket-created",
        "pane-created",
    ] {
        let root = TempDir::new().unwrap();
        let mut caller = spawn_case(root.path(), stage, true);
        let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
        let case = Path::new(&case);
        let dir = case.join(Uuid::from_u128(2).to_string());
        wait(|| dir.join("guardian-fault.json").exists().then_some(()));
        let identity: GuardianIdentity =
            serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
        let children: Vec<ProcessEvidence> = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            identity.process.pid, identity.process.pid
        ))
        .unwrap()
        .split_whitespace()
        .filter_map(|pid| ownership::process_evidence(pid.parse().unwrap()).ok())
        .collect();
        terminate(&identity.process);
        caller.kill().unwrap();
        caller.wait().unwrap();
        wait(|| {
            SessionHost::recover(case, &identity.intent)
                .is_err()
                .then_some(())
        });
        for child in children {
            terminate(&child);
        }
        assert!(
            !serde_json::from_slice::<HostObservation>(
                &std::fs::read(dir.join("state.json")).unwrap()
            )
            .unwrap()
            .kernel_wait_echild
        );
    }
}

struct StartGate<'a> {
    inner: &'a mut Authority,
    dir: PathBuf,
    deny: bool,
    collide: bool,
}
impl LaunchAuthority for StartGate<'_> {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        self.inner.record_launch_intent(i)
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        self.inner.record_guardian(i)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        if self.deny {
            anyhow::bail!("simulated caller fsync refusal")
        }
        if self.collide {
            durable_new(&self.dir.join("tmux.sock"), b"foreign socket sentinel")?;
        }
        self.inner.record_start_intent(i, g)
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        self.inner.record_started(o)
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn failed_durable_start_and_socket_collision_never_launch_native_child() {
    for collide in [false, true] {
        let mut c = Case::new("interactive", false);
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: c.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&c.manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + 1000,
        };
        let dir = c.temp.path().join(c.id.to_string());
        let mut gate = StartGate {
            inner: &mut c.authority,
            dir: dir.clone(),
            deny: !collide,
            collide,
        };
        assert!(
            SessionHost::launch(
                c.temp.path(),
                c.id,
                &c.manifest,
                p,
                &c.local,
                &c.adapter,
                &mut gate
            )
            .is_err()
        );
        assert!(!dir.join("fixture.claim").exists());
        assert!(!dir.join("pane-exec.claim").exists());
        let intent: LaunchIntent = serde_json::from_slice(
            &std::fs::read(c.temp.path().join("caller-intent.json")).unwrap(),
        )
        .unwrap();
        let mut h = SessionHost::recover(c.temp.path(), &intent).unwrap();
        assert!(
            h.wait_stopped(Duration::from_secs(5))
                .unwrap()
                .kernel_wait_echild
        );
        if collide {
            assert_eq!(
                std::fs::read(dir.join("tmux.sock")).unwrap(),
                b"foreign socket sentinel"
            );
        } else {
            assert!(!dir.join("start.request").exists());
        }
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn existing_pane_is_refused_and_never_used_for_the_agent() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "socket-created", true);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    let dir = case.join(Uuid::from_u128(2).to_string());
    wait(|| dir.join("guardian-fault.json").exists().then_some(()));
    let identity: GuardianIdentity =
        serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", identity.process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let status = Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(dir.join("tmux.sock"))
        .args(["new-session", "-d", "-s", "aq", "/bin/sleep", "10"])
        .status()
        .unwrap();
    assert!(status.success());
    let fd = ownership::verified_pidfd(&identity.process).unwrap();
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    let stopped = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(stopped.kernel_wait_echild);
    assert!(stopped.stop_reason.unwrap().contains("startup failed"));
    assert!(!dir.join("fixture.claim").exists());
    assert!(!dir.join("pane-exec.claim").exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn local_configuration_refusals_happen_before_durable_launch_or_process() {
    for case in ["version", "credentials", "context", "hard-resources"] {
        let mut c = Case::new("interactive", false);
        match case {
            "version" => c.local.tmux_version = "tmux unsupported".into(),
            "credentials" => c.local.credential_profiles.clear(),
            "hard-resources" => {
                c.local.resource_policy = ResourcePolicy::RequireQualifiedEnvironment(
                    c.manifest.harness.environment.clone(),
                )
            }
            "context" => std::fs::write(&c.local.context_material[0].path, b"changed").unwrap(),
            _ => unreachable!(),
        }
        assert!(c.launch(1000).is_err());
        assert!(!c.temp.path().join("caller-intent.json").exists());
        assert!(!c.temp.path().join(c.id.to_string()).exists());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewals_are_monotonic_and_resource_cost_is_observed() {
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(1000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 2000,
    };
    h.renew(&permit).unwrap();
    h.renew(&permit).unwrap();
    let mut changed = permit.clone();
    changed.expires_boot_ms += 1;
    assert!(h.renew(&changed).is_err());
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    let state = h.observe().unwrap();
    let pane = state.pane.unwrap();
    let mut rss = serde_json::Map::new();
    for (name, pid) in [
        ("guardian", state.guardian.process.pid),
        ("tmux", pane.server.pid),
        ("pane", pane.pane.pid),
    ] {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        rss.insert(name.into(), kb.into());
    }
    println!(
        "qualification_resource_rss_kib={}",
        serde_json::Value::Object(rss)
    );
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn malformed_renewal_stops_and_wall_limit_cannot_be_extended() {
    for malformed in [false, true] {
        let mut c = Case::new("interactive", false);
        if !malformed {
            c.manifest.resources.wall_time_ms = 600;
            c.manifest.resources.stop_grace_ms = 100;
        }
        let mut h = c.launch(500).unwrap();
        if malformed {
            std::fs::write(h.directory().join("renew.request"), b"{not a directive}").unwrap();
        } else {
            let now = ownership::raw_boot_ms();
            let p = ExecutionPermit {
                session: c.manifest.bindings.session.clone(),
                manifest_digest: canonical_digest(&c.manifest).unwrap(),
                sequence: 2,
                issued_boot_ms: now,
                expires_boot_ms: now + 2000,
            };
            assert!(h.renew(&p).is_err());
        }
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        if malformed {
            assert!(s.stop_reason.unwrap().contains("running validation failed"));
        } else {
            assert!(s.stop_reason.unwrap().contains("expired"));
        }
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewal_compare_publish_is_serialized_while_guardian_is_stopped() {
    use std::sync::Barrier;
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(5000).unwrap();
    let fd = ownership::verified_pidfd(&h.identity().process).unwrap();
    ownership::signal(&fd, libc::SIGSTOP).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", h.identity().process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 6000,
    };
    let mut changed = p.clone();
    changed.expires_boot_ms += 1;
    let barrier = Barrier::new(3);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            h.renew(&p)
        });
        let b = scope.spawn(|| {
            barrier.wait();
            h.renew(&changed)
        });
        barrier.wait();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.is_ok(), b.is_ok());
    let accepted = if a.is_ok() {
        p.clone()
    } else {
        changed.clone()
    };
    let rejected = if a.is_ok() { changed } else { p };
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    let mut higher = accepted.clone();
    higher.sequence = 3;
    h.renew(&higher).unwrap();
    assert!(h.renew(&accepted).is_err());
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn helper_expiry_survives_caller_death_without_recovery_control() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "start-receipt-durable", false);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    wait(|| case.join("caller-checkpoint").exists().then_some(()));
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap()).unwrap();
    caller.kill().unwrap();
    caller.wait().unwrap();
    let dir = case.join(intent.launch_id.to_string());
    let state = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(state.kernel_wait_echild);
    assert!(state.stop_reason.unwrap().contains("expired"));
    let h = SessionHost::recover(case, &intent).unwrap();
    assert_eq!(h.stopped_proof().unwrap().guardian().intent, intent);
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn sustained_adopted_child_exits_cannot_starve_expiry_or_stop_proof() {
    let mut c = Case::new("fork-flood", false);
    c.manifest.resources.stop_grace_ms = 100;
    let mut h = c.launch(700).unwrap();
    wait(|| {
        std::fs::read_to_string(h.directory().join("producer-counter"))
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 16)
    });
    let producer: ProcessEvidence = serde_json::from_slice(
        &std::fs::read(h.directory().join("producer-process.json")).unwrap(),
    )
    .unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.reaped_count >= 16);
    assert!(s.stop_reason.unwrap().contains("expired"));
    assert!(ownership::process_evidence(producer.pid).is_err());
}
