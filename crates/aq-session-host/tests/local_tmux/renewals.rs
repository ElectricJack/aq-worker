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
        // Give the guardian time to consume the malformed request before independent expiry.
        let mut h = c.launch(if malformed { 5000 } else { 500 }).unwrap();
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
