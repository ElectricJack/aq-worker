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
fn harness_waits_until_the_guardian_records_its_pane() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case_mode(root.path(), "pane-created", true, "exit");
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let dir = Path::new(&case).join(Uuid::from_u128(2).to_string());
    wait(|| dir.join("guardian-fault.json").exists().then_some(()));
    let identity: GuardianIdentity =
        serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
    // A harness that exits at once must not run while the guardian, held
    // after new-session, has not yet recorded the live pane identity.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!dir.join("fixture.claim").exists());
    let fd = ownership::verified_pidfd(&identity.process).unwrap();
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    let stopped = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(stopped.kernel_wait_echild && stopped.pane.is_some());
    assert_eq!(stopped.stop_reason.as_deref(), Some("harness pane exited"));
    assert!(dir.join("fixture.claim").exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
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
fn permit_expiry_is_independent_and_late_renewal_cannot_revive() {
    let mut c = Case::new("interactive", false);
    // The host must reach Running before this case can verify independent expiry.
    let mut h = c.launch(2_000).unwrap();
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
