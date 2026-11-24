#[test]
#[ignore = "subprocess fixture, called by local crash tests only"]
fn launch_child_fixture() {
    let Ok(root) = std::env::var("AQ_HOST_CASE_ROOT") else {
        return;
    };
    let temp = TempDir::new_in(&root).unwrap();
    let mode = std::env::var("AQ_HOST_CASE_MODE").unwrap_or_else(|_| "interactive".into());
    let mut c = Case::at(temp, &mode, false);
    std::fs::write(
        Path::new(&root).join("case-root"),
        c.temp.path().to_str().unwrap(),
    )
    .unwrap();
    c.authority.pause = std::env::var("AQ_HOST_CALLER_PAUSE").ok();
    let started = Instant::now();
    let handle = c.launch(1500);
    durable_new(
        &c.temp.path().join("caller-launch-result.json"),
        &serde_json::to_vec(&serde_json::json!({
            "ok": handle.is_ok(),
            "error": handle.as_ref().err().map(|e| format!("{e:#}")),
            "elapsed_ms": started.elapsed().as_millis(),
            "permit_ms": 1500,
            "caller_stage": std::env::var("AQ_HOST_CALLER_PAUSE").ok(),
            "guardian_stage": std::env::var("AQ_SESSION_HOST_FAULT_STAGE").ok(),
        }))
        .unwrap(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    std::mem::forget(c);
}
fn spawn_case(root: &Path, stage: &str, guardian_fault: bool) -> Child {
    spawn_case_mode(root, stage, guardian_fault, "interactive")
}
fn spawn_case_mode(root: &Path, stage: &str, guardian_fault: bool, mode: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "launch_child_fixture",
            "--test-threads=1",
        ])
        .env("AQ_HOST_CASE_ROOT", root)
        .env("AQ_HOST_CASE_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if guardian_fault {
        command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
    } else {
        command.env("AQ_HOST_CALLER_PAUSE", stage);
    }
    command.spawn().unwrap()
}
struct StageMarker<'a> {
    stage: &'a str,
    path: PathBuf,
}

fn wait_crash_marker(
    root: &mut Option<TempDir>,
    caller: &mut Child,
    case: &Path,
    marker: StageMarker,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if marker.path.exists() {
            return;
        }
        let result = std::fs::read_to_string(case.join("caller-launch-result.json")).ok();
        if result.is_some() || Instant::now() >= deadline {
            let stage = marker.stage;
            let retained = root.take().unwrap().keep();
            let _ = caller.kill();
            let _ = caller.wait();
            panic!(
                "synthetic crash stage {stage}: marker not reached; launch_result={result:?}; retained={}",
                retained.display()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
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
        let mut root = Some(TempDir::new().unwrap());
        let root_path = root.as_ref().unwrap().path().to_path_buf();
        let mut caller = spawn_case(&root_path, stage, false);
        let case = wait(|| std::fs::read_to_string(root_path.join("case-root")).ok());
        let case = Path::new(&case);
        wait_crash_marker(
            &mut root,
            &mut caller,
            case,
            StageMarker {
                stage,
                path: case.join("caller-checkpoint"),
            },
        );
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
        let mut root = Some(TempDir::new().unwrap());
        let root_path = root.as_ref().unwrap().path().to_path_buf();
        let mut caller = spawn_case(&root_path, stage, true);
        let case = wait(|| std::fs::read_to_string(root_path.join("case-root")).ok());
        let case = Path::new(&case);
        let dir = case.join(Uuid::from_u128(2).to_string());
        wait_crash_marker(
            &mut root,
            &mut caller,
            case,
            StageMarker {
                stage,
                path: dir.join("guardian-fault.json"),
            },
        );
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
