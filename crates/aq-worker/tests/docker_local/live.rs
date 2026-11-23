// ---------------------------------------------------------------------------
// Live qualification against the dedicated sandbox daemon (`--ignored`).

const LIVE_IMAGE: &str = "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for live docker-local tests"))
}

/// Live tests share one daemon and one of them pauses it, so they run one at
/// a time regardless of the harness's thread count.
static LIVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Live {
    _serial: tokio::sync::MutexGuard<'static, ()>,
    backend: DockerLocalBackend<FaultTransport<UnixSocketTransport>>,
    faults: FaultTransport<UnixSocketTransport>,
    node: Uuid,
    digest: TemplateDigest,
}

async fn live(runner: &[&str], edit: impl FnOnce(&mut DockerLocalConfig)) -> Live {
    let serial = LIVE_LOCK.lock().await;
    let socket = PathBuf::from(live_env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = std::env::var("AQ_DOCKER_LOCAL_IMAGE").unwrap_or_else(|_| LIVE_IMAGE.into());
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let mut config = DockerLocalConfig::new(socket.clone());
    config.templates.push(AllowedTemplate {
        image,
        digest: digest.clone(),
    });
    config.runner_command = Some(runner.iter().map(|arg| arg.to_string()).collect());
    config.api_timeout_ms = 30_000;
    config.stop_grace_secs = 1;
    // SBX20: a dev daemon whose storage shares `/` must opt in explicitly.
    config.shared_root_fs = std::env::var_os("AQ_DOCKER_LOCAL_SHARED_ROOT_FS").is_some();
    edit(&mut config);
    let faults = FaultTransport::new(UnixSocketTransport::new(socket));
    let mut backend = DockerLocalBackend::connect(config, faults.clone())
        .await
        .expect("connect to the dedicated sandbox daemon");
    backend.pull_template(&digest).await.expect("pull template");
    Live {
        _serial: serial,
        backend,
        faults,
        node: Uuid::new_v4(),
        digest,
    }
}

impl Live {
    fn request(&self, attempt: u128) -> CreateRequest {
        CreateRequest::new(
            Ownership {
                node_id: self.node,
                boot_id: Uuid::from_u128(2),
                attempt_id: Uuid::from_u128(attempt),
            },
            self.digest.clone(),
            3_600_000,
            vec![Capability::PersistentDiskDuringAttempt],
        )
        .with_resources(LIMITS)
    }

    /// Best-effort cleanup scoped to this run's node label.
    async fn cleanup(&mut self) {
        let Ok(owned) = self.backend.list_owned(self.node).await else {
            return;
        };
        for entry in owned {
            let _ = self.backend.stop(&entry.sandbox).await;
            if let Ok(release) =
                DestroyAuthorization::operator_released(entry.sandbox.clone(), "live-cleanup")
            {
                let _ = self.backend.destroy(&entry.sandbox, &release).await;
            }
        }
    }

    async fn wait_for_file(&mut self, sandbox: &SandboxIdentity, path: &str) -> Vec<u8> {
        for _ in 0..100 {
            if let Ok(data) = self.backend.read_file(sandbox, path).await {
                return data;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{path} never appeared");
    }
}

const LIVE_RUNNER: &[&str] = &[
    "sh",
    "-c",
    "test -s /aq/private/material && printf 'fixture-complete\\n' > /aq/work/fixture-output",
];

struct LiveFixture {
    live: Live,
    retained: BTreeMap<SandboxIdentity, Vec<u8>>,
}
impl BackendFixture for LiveFixture {
    type Backend = DockerLocalBackend<FaultTransport<UnixSocketTransport>>;
    fn backend(&mut self) -> &mut Self::Backend {
        &mut self.live.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        self.live.request(case + 10)
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        let (method, path_prefix, effect) = match fault {
            Fault::CrashAfterCreate => (
                DockerMethod::Post,
                "/v1.44/containers/create",
                Effect::DropReplyAfterApply,
            ),
            Fault::DestroyFailure => (
                DockerMethod::Delete,
                "/v1.44/containers/",
                Effect::FailBefore(500),
            ),
            other => panic!("live fixture does not inject {other:?}"),
        };
        self.live.faults.script(Rule {
            method,
            path_prefix: path_prefix.into(),
            effect,
        });
    }
    async fn observe_fixture(&mut self, sandbox: &SandboxIdentity) {
        let output = self
            .live
            .wait_for_file(sandbox, "/aq/work/fixture-output")
            .await;
        assert_eq!(output, b"fixture-complete\n");
    }
    async fn verify_and_retain(&mut self, sandbox: &SandboxIdentity) -> DestroyAuthorization {
        let output = self
            .live
            .wait_for_file(sandbox, "/aq/work/fixture-output")
            .await;
        assert_eq!(output, b"fixture-complete\n");
        self.retained.insert(sandbox.clone(), output);
        DestroyAuthorization::retained(
            sandbox.clone(),
            format!("live-retention/{}", sandbox.backend_id),
        )
        .unwrap()
    }
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET"]
async fn live_conformance() {
    let mut fixture = LiveFixture {
        live: live(LIVE_RUNNER, |_| {}).await,
        retained: BTreeMap::new(),
    };
    conformance::run_all(&mut fixture).await;
    let leftover = fixture.live.backend.list_owned(fixture.live.node).await;
    fixture.live.cleanup().await;
    assert_eq!(leftover.map(|owned| owned.len()), Ok(0));
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET"]
async fn live_launch_policy_applied() {
    let probe = "grep -E '^(CapEff|NoNewPrivs):' /proc/self/status > /aq/work/policy; \
                 for f in cpu.max memory.max pids.max; do \
                 printf '%s: %s\\n' \"$f\" \"$(cat /sys/fs/cgroup/$f)\" >> /aq/work/policy; done";
    let mut live = live(&["sh", "-c", probe], |_| {}).await;
    let request = live.request(1);
    let sandbox = live.backend.create(&request).await.unwrap();
    live.backend.start_runner(&sandbox).await.unwrap();
    let policy = String::from_utf8(live.wait_for_file(&sandbox, "/aq/work/policy").await).unwrap();
    live.cleanup().await;
    let line = |key: &str| {
        policy
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .map(str::trim)
            .unwrap_or_else(|| panic!("{key} missing from probe output:\n{policy}"))
            .to_string()
    };
    assert_eq!(line("CapEff"), "0000000000000000", "{policy}");
    assert_eq!(line("NoNewPrivs"), "1", "{policy}");
    assert_eq!(line("cpu.max"), "50000 100000", "{policy}");
    assert_eq!(line("memory.max"), "67108864", "{policy}");
    assert_eq!(line("pids.max"), "32", "{policy}");
}

fn signal_daemon(signal: &str) {
    for pid in live_env("AQ_DOCKER_LOCAL_DAEMON_PID")
        .split([',', ' '])
        .filter(|p| !p.is_empty())
    {
        let status = std::process::Command::new("kill")
            .args([signal, pid])
            .status()
            .unwrap();
        assert!(status.success(), "kill {signal} {pid}");
    }
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET, AQ_DOCKER_LOCAL_DAEMON_PID"]
async fn live_wsl_suspend_resume_reconciles_sandboxes() {
    live_env("AQ_DOCKER_LOCAL_DAEMON_PID");
    let mut live = live(&["sh", "-c", "sleep 600"], |config| {
        config.api_timeout_ms = 3000
    })
    .await;
    let request = live.request(1);
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    let sandbox = live.backend.create(&request).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    live.backend
        .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec()))
        .await
        .unwrap();
    live.backend.start_runner(&sandbox).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    live.faults.clear_requests();

    signal_daemon("-STOP");
    let suspended = tokio::time::timeout(
        Duration::from_secs(30),
        suspended_is_unknown(&mut live.backend, &sandbox, &release),
    )
    .await;
    signal_daemon("-CONT");
    suspended.expect("suspended checks hung");
    assert!(
        !live
            .faults
            .requests()
            .iter()
            .any(|(method, _)| *method == DockerMethod::Delete),
        "no destroy during the pause"
    );
    resumed_reconciles(&mut live.backend, &journal, &sandbox).await;
    assert_eq!(
        live.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running,
        "the container keeps running across a daemon pause"
    );
    live.cleanup().await;
    assert_eq!(
        live.backend.list_owned(live.node).await.map(|o| o.len()),
        Ok(0)
    );
}
