#[test]
fn sandbox_run_cli_requires_frozen_manifest_and_state() {
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .args(["sandbox-run", "--help"])
        .output()
        .unwrap();
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("--manifest") && text.contains("--state-dir"));
    assert!(
        !std::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("sandbox-run")
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[path = "support/sandbox_runner.rs"]
mod support;
use aq_worker::native::runner::sandbox::RunOutcome;
use support::*;

#[tokio::test]
async fn runner_cannot_report_before_handoff_receipts() {
    for mode in [Mode::MissingSource, Mode::MissingResult, Mode::Complete] {
        let f = Fixture::new(false, 5000);
        let mut central = Central::new(mode);
        let outcome = f.run(&mut central).await.unwrap();
        if matches!(mode, Mode::Complete) {
            assert_eq!(outcome, RunOutcome::Finished);
            assert_eq!(central.finishes, 1);
        } else {
            assert!(matches!(
                outcome,
                RunOutcome::Preserved {
                    stop_proven: true,
                    ..
                }
            ));
            assert_eq!(central.finishes, 0);
        }
        assert_eq!(central.retentions, 1);
        assert!(f.local.workspace.join("output.txt").exists());
        assert!(
            f.run(&mut central).await.is_err(),
            "retained handoff must never relaunch"
        );
    }
}
#[tokio::test]
async fn renewal_loss_stops_and_preserves_without_relaunch() {
    for mode in [Mode::Refuse, Mode::Stall] {
        let f = Fixture::new(true, 5000);
        let mut central = Central::new(mode);
        let outcome = f.run(&mut central).await.unwrap();
        assert!(
            matches!(
                outcome,
                RunOutcome::Preserved {
                    stop_proven: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(central.renewals > 0);
        assert_eq!(central.finishes, 0);
        assert_eq!(central.retentions, 0);
        assert!(f.local.workspace.join("started.claim").exists());
        let counter = f.local.workspace.join("descendant-counter");
        let before = std::fs::read(&counter).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            before,
            std::fs::read(counter).unwrap(),
            "escaped descendant kept writing after proof"
        );
        assert!(f.run(&mut central).await.is_err());
        assert!(f.dir.path().join("sandbox-run/stopped.json").exists());
    }
}

#[tokio::test]
async fn delegated_permit_expiry_caps_initial_grant() {
    let mut f = Fixture::new(true, 5000);
    f.permit.expires_at_ms = 2200;
    let mut central = Central::new(Mode::Stall);

    let outcome = f.run(&mut central).await.unwrap();
    assert!(matches!(
        outcome,
        RunOutcome::Preserved {
            stop_proven: true,
            ..
        }
    ));
    let initial: aq_session_host::ExecutionPermit = serde_json::from_slice(
        &std::fs::read(f.dir.path().join("sandbox-run/initial-permit.json")).unwrap(),
    )
    .unwrap();
    assert!(
        initial.expires_boot_ms <= f.frozen.sent_boot_ns / 1_000_000 + 1200,
        "grant expiry must not extend shorter delegated authority"
    );
}

#[test]
fn frozen_handoff_rejects_reboot_changed_manifest_and_directory_copy() {
    let f = Fixture::new(false, 5000);
    f.frozen.validate_local(f.dir.path()).unwrap();
    let mut wrong = f.frozen.clone();
    wrong.kernel_boot_id = uuid::Uuid::new_v4().to_string();
    assert!(wrong.validate_local(f.dir.path()).is_err());
    let mut wrong = f.frozen.clone();
    wrong.manifest.resources.wall_time_ms += 1;
    assert!(wrong.validate_local(f.dir.path()).is_err());
    let mut expired = f.frozen.clone();
    expired.sent_boot_ns -= 10_000_000_000;
    expired.received_boot_ns -= 10_000_000_000;
    assert!(
        expired.validate_local(f.dir.path()).is_err(),
        "elapsed suspend time must not reset TTL"
    );
    let other = tempfile::tempdir().unwrap();
    assert!(f.frozen.validate_local(other.path()).is_err());
}

#[tokio::test]
async fn renewal_keeps_same_guardian_alive_past_original_deadline() {
    let f = Fixture::new(true, 5000);
    let mut central = Renewing::new(&f, RenewalMode::Renew);
    let release = async {
        tokio::time::sleep(std::time::Duration::from_millis(6000)).await;
        assert!(f.local.workspace.join("descendant-counter").exists());
        std::fs::write(f.local.workspace.join("release-descendant"), b"").unwrap();
        std::fs::write(f.local.workspace.join("release"), b"").unwrap();
    };
    let (outcome, ()) = tokio::join!(f.run(&mut central), release);
    assert_eq!(outcome.unwrap(), RunOutcome::Finished);
    assert!(central.central.renewals >= 2);
    let proof: serde_json::Value = serde_json::from_slice(
        &std::fs::read(f.dir.path().join("sandbox-run/stopped.json")).unwrap(),
    )
    .unwrap();
    assert!(proof["observation"]["permit_sequence"].as_u64().unwrap() > 1);
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("sandbox-run/hosts"))
            .unwrap()
            .count(),
        1
    );
    let journal = std::fs::read_to_string(f.dir.path().join("sandbox-run/handoff.json")).unwrap();
    assert!(
        !journal.contains("fixture-worker-secret")
            && !journal.contains(f.permit.credential.expose())
    );
}

#[tokio::test]
async fn renewal_replay_wrong_nonce_and_readoption_never_extend_authority() {
    for mode in [
        RenewalMode::Replay,
        RenewalMode::BadNonce,
        RenewalMode::Awaiting,
    ] {
        let f = Fixture::new(true, 5000);
        let mut central = Renewing::new(&f, mode);
        let outcome = f.run(&mut central).await.unwrap();
        assert!(
            matches!(
                outcome,
                RunOutcome::Preserved {
                    stop_proven: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(central.central.finishes, 0);
        assert!(!f.dir.path().join("sandbox-run/renewal-3.json").exists());
        if !matches!(mode, RenewalMode::Replay) {
            assert!(!f.dir.path().join("sandbox-run/renewal-2.json").exists());
        }
    }
}

#[tokio::test]
async fn concurrent_handoff_has_only_one_writer_launch() {
    let f = Fixture::new(true, 5000);
    let mut a = Central::new(Mode::Stall);
    let mut b = Central::new(Mode::Stall);
    let (a, b) = tokio::join!(f.run(&mut a), f.run(&mut b));
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("sandbox-run/hosts"))
            .unwrap()
            .count(),
        1
    );
}

async fn wait_file(path: &std::path::Path) {
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < until,
            "fixture did not start: {}",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
#[tokio::test]
async fn unknown_stop_retains_without_reporting_or_claiming_proof() {
    let f = Fixture::new(true, 3000);
    let mut central = Central::new(Mode::Stall);
    let directory = f
        .dir
        .path()
        .join("sandbox-run/hosts")
        .join(f.frozen.launch_id.to_string());
    let corrupt_stop = async {
        wait_file(&f.local.workspace.join("descendant-counter")).await;
        // The host rejects a malformed directive and stops independently. The
        // runner cannot persist its exact stop request, so must preserve uncertainty.
        std::fs::create_dir(directory.join("stop.request")).unwrap();
    };
    let (outcome, ()) = tokio::join!(f.run(&mut central), corrupt_stop);
    assert!(matches!(
        outcome.unwrap(),
        RunOutcome::Preserved {
            stop_proven: false,
            ..
        }
    ));
    assert_eq!(central.retentions, 0);
    assert_eq!(central.finishes, 0);
    assert!(!f.dir.path().join("sandbox-run/stopped.json").exists());
    // Let the independent guardian finish; never leave fixture writers behind.
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let state: aq_session_host::HostObservation =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        if state.phase == aq_session_host::HostPhase::Stopped {
            assert!(state.kernel_wait_echild);
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "fixture guardian failed to stop"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(f.local.workspace.join("descendant-counter").exists());
    assert!(f.run(&mut central).await.is_err());
}
#[tokio::test]
async fn tmux_exit_requires_descendant_stop_proof() {
    let f = Fixture::new(true, 5000);
    let mut central = Central::new(Mode::Stall);
    let kill_tmux = async {
        wait_file(&f.local.workspace.join("descendant-counter")).await;
        let socket = f
            .dir
            .path()
            .join("sandbox-run/hosts")
            .join(f.frozen.launch_id.to_string())
            .join("tmux.sock");
        assert!(
            std::process::Command::new("/usr/bin/tmux")
                .arg("-S")
                .arg(socket)
                .args(["kill-server"])
                .status()
                .unwrap()
                .success()
        );
    };
    let (outcome, ()) = tokio::join!(f.run(&mut central), kill_tmux);
    assert!(matches!(
        outcome.unwrap(),
        RunOutcome::Preserved {
            stop_proven: true,
            ..
        }
    ));
    assert_eq!(central.finishes, 0);
    let proof: serde_json::Value = serde_json::from_slice(
        &std::fs::read(f.dir.path().join("sandbox-run/stopped.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(proof["observation"]["kernel_wait_echild"], true);
    let counter = f.local.workspace.join("descendant-counter");
    let stopped = std::fs::read(&counter).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(stopped, std::fs::read(counter).unwrap());
}
#[test]
fn production_cli_refuses_unwired_transport_before_launch() {
    let f = Fixture::new(false, 5000);
    let config = f.dir.path().join("worker.toml");
    f.worker.save(&config).unwrap();
    let manifest = f.dir.path().join("manifest.json");
    aq_session_host::durable_new(&manifest, &serde_json::to_vec(&f.frozen).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(config)
        .arg("sandbox-run")
        .arg("--manifest")
        .arg(manifest)
        .arg("--state-dir")
        .arg(f.dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("transport is unavailable")
    );
    assert!(!f.dir.path().join("sandbox-run").exists());
    assert!(!f.local.workspace.join("started.claim").exists());
}

#[tokio::test]
async fn cancellation_preserves_and_default_local_permit_is_at_most_120_seconds() {
    let mut f = Fixture::new(true, 120_000);
    f.frozen.authority.hard_deadline_ms = 201_000;
    f.permit.hard_deadline_ms = 201_000;
    f.frozen.manifest.resources.wall_time_ms = 200_000;
    f.frozen.authority.grant.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.frozen.manifest).unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let cancel = async {
        wait_file(&f.local.workspace.join("descendant-counter")).await;
        tx.send(true).unwrap();
    };
    let mut central = Central::new(Mode::Stall);
    let options = aq_worker::native::runner::sandbox::RunOptions::default();
    assert_eq!(options.renewal_interval, std::time::Duration::from_secs(15));
    let (outcome, ()) = tokio::join!(
        aq_worker::native::runner::sandbox::run(
            &f.worker,
            &f.frozen,
            f.permit.clone(),
            f.dir.path(),
            &f.local,
            &f.adapter,
            &mut central,
            rx,
            options
        ),
        cancel
    );
    assert_eq!(
        outcome.unwrap(),
        RunOutcome::Preserved {
            reason: "cancelled",
            stop_proven: true
        }
    );
    let initial: aq_session_host::ExecutionPermit = serde_json::from_slice(
        &std::fs::read(f.dir.path().join("sandbox-run/initial-permit.json")).unwrap(),
    )
    .unwrap();
    assert!(initial.expires_boot_ms - initial.issued_boot_ms <= 120_000);
    assert_eq!(central.finishes, 0);
    assert_eq!(central.renewals, 0);
}
