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
    assert!(text.contains("--permit-file") && text.contains("--server-url"));
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
        let mut f = Fixture::new(true, 5000);
        f.adapter.descendant = false;
        let mut central = Central::new(mode);
        let release = async {
            // A short-lived pane can exit before the host acknowledges launch.
            // Exercise receipt handling only after that durable acknowledgement.
            wait_file(&f.dir.path().join("sandbox-run/started.json")).await;
            std::fs::write(f.local.workspace.join("release"), b"").unwrap();
        };
        let (outcome, ()) = tokio::join!(f.run(&mut central), release);
        let outcome = outcome.unwrap();
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
fn production_cli_refuses_missing_prerequisites_or_unwired_transport_before_launch() {
    let f = Fixture::new(false, 5000);
    let config = f.dir.path().join("worker.toml");
    f.worker.save(&config).unwrap();
    let manifest = f.dir.path().join("manifest.json");
    aq_session_host::durable_new(&manifest, &serde_json::to_vec(&f.frozen).unwrap()).unwrap();
    let files = aq_worker::native::runner::sandbox::private::RunnerFiles::create(
        &f.dir.path().join("runner-private"),
    )
    .unwrap();
    files
        .write_secret("permit.json", &serde_json::to_vec(&f.permit).unwrap())
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(config)
        .arg("sandbox-run")
        .arg("--manifest")
        .arg(manifest)
        .arg("--state-dir")
        .arg(f.dir.path())
        .arg("--permit-file")
        .arg(files.root().join("permit.json"))
        .args(["--server-url", "http://127.0.0.1:1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    // User/capability bootstrap precedes the async transport check. This
    // fixture must also verify explicit refusal outside the sandbox image.
    assert!(
        [
            "required sandbox OS account unavailable",
            "sandbox requires SETUID/SETGID/KILL capabilities",
            "runner bootstrap requires root entrypoint",
            "B2 receipt codec is unavailable",
        ]
        .iter()
        .any(|reason| stderr.contains(reason)),
        "unexpected sandbox preflight error: {stderr}"
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

#[tokio::test]
async fn runner_needs_no_node_credential() {
    // Hold the fixture agent until the host durably records launch; an
    // immediately exiting pane can otherwise stop before startup is observed.
    let mut f = Fixture::new(true, 5000);
    f.adapter.descendant = false;
    f.worker.credential.clear();
    struct PermitOnly(Central);
    impl aq_worker::native::runner::sandbox::RunnerTransport for PermitOnly {
        fn preflight(&mut self, worker: &aq_worker::config::WorkerConfig) -> anyhow::Result<()> {
            assert!(worker.credential.is_empty());
            Ok(())
        }
        fn renew<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            request: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
            nonce: uuid::Uuid,
        ) -> aq_worker::native::runner::sandbox::TransportFuture<
            'a,
            aq_worker::native::runner::sandbox::RenewalOutcome,
        > {
            self.0.renew(worker, request, nonce)
        }
        fn retain<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            stopped: &'a aq_worker::native::runner::sandbox::StoppedAttempt,
        ) -> aq_worker::native::runner::sandbox::TransportFuture<
            'a,
            aq_worker::native::runner::sandbox::RetentionReceipts,
        > {
            self.0.retain(worker, stopped)
        }
        fn finished<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            retained: &'a aq_worker::native::runner::sandbox::RetainedAttempt,
        ) -> aq_worker::native::runner::sandbox::TransportFuture<
            'a,
            aq_worker::native::runner::sandbox::FinishedReceipt,
        > {
            self.0.finished(worker, retained)
        }
    }
    let mut transport = PermitOnly(Central::new(Mode::Complete));
    let release = async {
        wait_file(&f.dir.path().join("sandbox-run/started.json")).await;
        std::fs::write(f.local.workspace.join("release"), b"").unwrap();
    };
    let (outcome, ()) = tokio::join!(f.run(&mut transport), release);
    assert_eq!(outcome.unwrap(), RunOutcome::Finished);
    assert_eq!(transport.0.finishes, 1);
}

#[tokio::test]
async fn runner_channel_refusal_stops_and_preserves() {
    use aq_worker::native::runner::sandbox::{
        auth::{RunnerHttp, RunnerRoute},
        permit_transport::{PermitTransport, RunnerExchange},
        *,
    };
    struct RefusingChannel;
    impl RunnerExchange for RefusingChannel {
        fn preflight(&mut self, _: &RunnerHttp) -> anyhow::Result<()> {
            Ok(())
        }
        fn renew<'a>(
            &'a mut self,
            http: &'a RunnerHttp,
            request: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
            _: uuid::Uuid,
        ) -> TransportFuture<'a, RenewalOutcome> {
            Box::pin(async move {
                let _ = http
                    .post(
                        RunnerRoute::Observations,
                        &request.permit.binding,
                        &serde_json::json!({"binding":request.permit.binding}),
                    )
                    .await;
                // Another channel revoked authority; renewal itself never completes.
                std::future::pending().await
            })
        }
        fn retain<'a>(
            &'a mut self,
            _: &'a RunnerHttp,
            _: &'a StoppedAttempt,
        ) -> TransportFuture<'a, RetentionReceipts> {
            Box::pin(async { panic!("authority loss must preserve before retention") })
        }
        fn finished<'a>(
            &'a mut self,
            _: &'a RunnerHttp,
            _: &'a RetainedAttempt,
        ) -> TransportFuture<'a, FinishedReceipt> {
            Box::pin(async { panic!("authority loss must never finish") })
        }
    }
    let mut f = Fixture::new(true, 5000);
    f.worker.credential.clear();
    let files = private::RunnerFiles::create(&f.dir.path().join("runner-private")).unwrap();
    files
        .write_secret("permit.json", &serde_json::to_vec(&f.permit).unwrap())
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(|| async { axum::http::StatusCode::FORBIDDEN });
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http =
        RunnerHttp::for_execution(&origin, &files.root().join("permit.json"), &f.frozen).unwrap();
    // A valid client for a different sandbox cannot launch this frozen attempt.
    let mut foreign_permit = f.permit.clone();
    foreign_permit.binding.sandbox_id = uuid::Uuid::new_v4();
    let foreign_files =
        private::RunnerFiles::create(&f.dir.path().join("foreign-private")).unwrap();
    foreign_files
        .write_secret("permit.json", &serde_json::to_vec(&foreign_permit).unwrap())
        .unwrap();
    let foreign_http = RunnerHttp::for_execution(
        &origin,
        &foreign_files.root().join("permit.json"),
        &f.frozen,
    )
    .unwrap();
    let mut foreign_transport = PermitTransport::new(foreign_http, RefusingChannel);
    assert!(f.run(&mut foreign_transport).await.is_err());
    assert!(!f.dir.path().join("sandbox-run").exists());
    let retained = f.local.workspace.join("retained-before-launch");
    std::fs::write(&retained, b"unretained work").unwrap();
    let mut transport = PermitTransport::new(http.clone(), RefusingChannel);
    let result = f.run(&mut transport).await.unwrap();
    server.abort();
    assert_eq!(
        result,
        RunOutcome::Preserved {
            reason: "runner authority lost",
            stop_proven: true
        }
    );
    assert!(http.authority_lost());
    assert_eq!(std::fs::read(retained).unwrap(), b"unretained work");
    assert!(
        f.run(&mut transport).await.is_err(),
        "preserved attempt must never relaunch"
    );
}

mod login {
    //! calm-delta-18.10.1: the C6b subscription login at sandbox launch level.
    use super::{support::*, wait_file};
    use aq_worker::native::runner::sandbox::{
        RunOptions, RunOutcome, RunnerTransport, SandboxLogin, TransportFuture,
        private::RunnerFiles,
        subscription_login::{
            HarnessExit, IssuedLogin, LoginFailure, LoginKind, LoginService, WriteBack,
        },
    };
    use std::path::PathBuf;
    use uuid::Uuid;
    use zeroize::Zeroizing;

    const LEASE: Uuid = Uuid::from_u128(0x18101);
    const TOKEN: &str = "sk-ant-oat01-fixture-subscription-token";

    /// A fake lease source and C5b server. Each write-back records whether any
    /// stopped/preserved record already existed: it must run before both.
    struct Leases {
        issued: Option<IssuedLogin>,
        run_dir: PathBuf,
        uploads: Vec<(Uuid, i64, Vec<u8>, bool)>,
    }
    impl Leases {
        fn new(f: &Fixture, kind: LoginKind, material: &[u8]) -> Self {
            Self {
                issued: Some(IssuedLogin {
                    kind,
                    lease_id: LEASE,
                    version: 7,
                    material: Zeroizing::new(material.to_vec()),
                }),
                run_dir: f.dir.path().join("sandbox-run"),
                uploads: Vec::new(),
            }
        }
    }
    impl LoginService for Leases {
        async fn fetch_newest(&mut self, _: LoginKind) -> anyhow::Result<IssuedLogin> {
            self.issued
                .take()
                .ok_or_else(|| anyhow::anyhow!("no newer login"))
        }
        async fn write_back(
            &mut self,
            lease_id: Uuid,
            base_version: i64,
            bytes: &[u8],
        ) -> anyhow::Result<WriteBack> {
            let recorded = ["stopped.json", "preserved.json"]
                .iter()
                .any(|name| self.run_dir.join(name).exists());
            self.uploads
                .push((lease_id, base_version, bytes.to_vec(), recorded));
            Ok(WriteBack::Accepted {
                version: base_version + 1,
                replayed: false,
            })
        }
    }
    fn codex_auth(last_refresh: &str) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
            "tokens":{"id_token":"offline-id","access_token":"offline-access",
                "refresh_token":"offline-refresh","account_id":"offline-account"},
            "last_refresh":last_refresh}),
        )
        .unwrap()
    }
    fn receipt(f: &Fixture) -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(f.dir.path().join("sandbox-run/login-writeback.json")).unwrap(),
        )
        .unwrap()
    }
    fn sealed(f: &Fixture) -> serde_json::Value {
        let path = f
            .dir
            .path()
            .join("sandbox-run/hosts")
            .join(f.frozen.launch_id.to_string())
            .join("launch.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    fn long_permit(f: &mut Fixture) {
        f.frozen.authority.hard_deadline_ms = 201_000;
        f.permit.hard_deadline_ms = 201_000;
        f.frozen.manifest.resources.wall_time_ms = 200_000;
        f.frozen.authority.grant.binding.launch_manifest_digest =
            aq_session_protocol::canonical_digest(&f.frozen.manifest).unwrap();
    }

    #[tokio::test]
    async fn cancel_writes_back_refreshed_login_and_wipes_before_preserve() {
        let mut f = Fixture::new(true, 120_000);
        long_permit(&mut f);
        let home = f.dir.path().join("codex-home");
        let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
        let mut leases = Leases::new(
            &f,
            LoginKind::CodexChatgptAuth,
            &codex_auth("2026-11-01T00:00:00Z"),
        );
        let refreshed = codex_auth("2026-11-04T00:00:00Z");
        let (tx, rx) = tokio::sync::watch::channel(false);
        let cancel = async {
            wait_file(&f.local.workspace.join("descendant-counter")).await;
            // The harness refreshed its login in CODEX_HOME, then is cancelled.
            std::fs::write(home.join("auth.json"), &refreshed).unwrap();
            tx.send(true).unwrap();
        };
        let mut central = Central::new(Mode::Stall);
        let (outcome, ()) = tokio::join!(
            aq_worker::native::runner::sandbox::run_with_login(
                &f.worker,
                &f.frozen,
                f.permit.clone(),
                f.dir.path(),
                &f.local,
                &f.adapter,
                &mut central,
                rx,
                RunOptions::default(),
                None,
                SandboxLogin {
                    service: &mut leases,
                    kind: LoginKind::CodexChatgptAuth,
                    codex_home: Some(home.clone()),
                    files: &files,
                    classify: |_| HarnessExit::Completed,
                },
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
        // One write-back of N+1 based on N, before any stopped/preserved record.
        assert_eq!(leases.uploads.len(), 1);
        let (lease, base, bytes, recorded) = &leases.uploads[0];
        assert_eq!(
            (*lease, *base, bytes, *recorded),
            (LEASE, 7, &refreshed, false)
        );
        // Wiped, and the byte-free receipt journaled in runner-only state.
        assert!(!home.exists());
        let receipt = receipt(&f);
        assert_eq!(receipt["outcome"], "accepted");
        assert_eq!(receipt["version"], 8);
        assert!(f.dir.path().join("sandbox-run/preserved.json").exists());
        // CODEX_HOME is the spawn environment; the login is never in argv.
        let sealed = sealed(&f);
        assert_eq!(
            sealed["command"]["environment"]["CODEX_HOME"],
            home.to_str().unwrap()
        );
        assert!(!sealed.to_string().contains("offline-refresh"));
    }

    /// Asserts at retention (the disk being preserved) that the login is gone.
    struct Retaining {
        central: Central,
        home: PathBuf,
        run_dir: PathBuf,
    }
    impl RunnerTransport for Retaining {
        fn preflight(&mut self, worker: &aq_worker::config::WorkerConfig) -> anyhow::Result<()> {
            self.central.preflight(worker)
        }
        fn renew<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            request: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
            nonce: Uuid,
        ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::RenewalOutcome> {
            self.central.renew(worker, request, nonce)
        }
        fn retain<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            stopped: &'a aq_worker::native::runner::sandbox::StoppedAttempt,
        ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::RetentionReceipts> {
            assert!(!self.home.exists(), "login must be wiped before preserve");
            assert!(self.run_dir.join("login-writeback.json").exists());
            self.central.retain(worker, stopped)
        }
        fn finished<'a>(
            &'a mut self,
            worker: &'a aq_worker::config::WorkerConfig,
            retained: &'a aq_worker::native::runner::sandbox::RetainedAttempt,
        ) -> TransportFuture<'a, aq_worker::native::runner::sandbox::FinishedReceipt> {
            self.central.finished(worker, retained)
        }
    }

    #[tokio::test]
    async fn completion_wipes_login_before_retention() {
        let f = Fixture::new(false, 5000);
        let home = f.dir.path().join("codex-home");
        let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
        let mut leases = Leases::new(
            &f,
            LoginKind::CodexChatgptAuth,
            &codex_auth("2026-11-01T00:00:00Z"),
        );
        let mut transport = Retaining {
            central: Central::new(Mode::Complete),
            home: home.clone(),
            run_dir: f.dir.path().join("sandbox-run"),
        };
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let outcome = aq_worker::native::runner::sandbox::run_with_login(
            &f.worker,
            &f.frozen,
            f.permit.clone(),
            f.dir.path(),
            &f.local,
            &f.adapter,
            &mut transport,
            rx,
            RunOptions::default(),
            None,
            SandboxLogin {
                service: &mut leases,
                kind: LoginKind::CodexChatgptAuth,
                codex_home: Some(home.clone()),
                files: &files,
                classify: |_| HarnessExit::Completed,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, RunOutcome::Finished);
        assert_eq!(transport.central.retentions, 1);
        assert!(leases.uploads.is_empty(), "unchanged login is not uploaded");
        assert_eq!(receipt(&f)["outcome"], "not_uploaded");
    }

    #[tokio::test]
    async fn claude_authentication_failure_is_final_credential_invalid() {
        let f = Fixture::new(false, 5000);
        let files = RunnerFiles::create(&f.dir.path().join("runner-files")).unwrap();
        let mut leases = Leases::new(&f, LoginKind::ClaudeOauthToken, TOKEN.as_bytes());
        let mut central = Central::new(Mode::Complete);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let outcome = aq_worker::native::runner::sandbox::run_with_login(
            &f.worker,
            &f.frozen,
            f.permit.clone(),
            f.dir.path(),
            &f.local,
            &f.adapter,
            &mut central,
            rx,
            RunOptions::default(),
            None,
            SandboxLogin {
                service: &mut leases,
                kind: LoginKind::ClaudeOauthToken,
                codex_home: None,
                files: &files,
                classify: |_| HarnessExit::AuthenticationFailed,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            RunOutcome::LoginFailed {
                failure: LoginFailure::CREDENTIAL_INVALID,
                stop_proven: true
            }
        );
        assert!(leases.uploads.is_empty());
        assert_eq!(central.retentions, 0, "a failed login attempt is preserved");
        assert!(f.dir.path().join("sandbox-run/login-failure.json").exists());
        // The token is only the spawned process environment: not argv, not the
        // manifest recipe environment.
        let sealed = sealed(&f);
        assert_eq!(
            sealed["command"]["environment"]["CLAUDE_CODE_OAUTH_TOKEN"],
            TOKEN
        );
        assert_eq!(sealed.to_string().matches(TOKEN).count(), 1);
    }
}
