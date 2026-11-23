fn wait<T>(f: impl FnMut() -> Option<T>) -> T {
    wait_named("local process predicate", f)
}
fn wait_named<T>(label: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "{label} timeout");
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
        if s == "launch-intent-durable" {
            self.check_private_publication()?;
        }
        if self.pause.as_deref() == Some(s) {
            durable_new(&self.root.join("caller-checkpoint"), s.as_bytes())?;
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}
impl Authority {
    fn check_private_publication(&self) -> Result<()> {
        let auth = self
            .root
            .join(Uuid::from_u128(2).to_string())
            .join("codex-home/auth.json");
        if auth.exists() {
            anyhow::ensure!(
                self.root.join("caller-intent.json").exists()
                    && std::fs::metadata(auth)?.len() == 0,
                "private auth published before durable intent"
            );
            durable_new(
                &self.root.join("auth-order.checked"),
                b"durable intent precedes private publication",
            )?;
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
            serde_json::from_str(include_str!("../fixtures/manifest.json")).unwrap();
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

fn io_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn io_setup() -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    io_setup_mode("terminal-journal")
}
fn io_setup_mode(
    mode: &str,
) -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::{Effect, Fence};
    let mut case = Case::new(mode, false);
    let host = case.launch(8000).unwrap();
    wait(|| {
        case.adapter
            .dir
            .join("fixture.claim")
            .exists()
            .then_some(())
    });
    let terminal = TerminalIdentity {
        session: case.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let io = host.bind_terminal(&terminal).unwrap();
    let now = io_now();
    let lease = ControllerLease {
        terminal: terminal.clone(),
        principal_id: Text::new("synthetic-owner").unwrap(),
        lease_id: Uuid::new_v4(),
        generation: 1,
        issued_at_ms: now,
        expires_at_ms: now + 6000,
        execution_expires_at_ms: now + 7000,
    };
    let fence = Fence {
        terminal,
        fence_id: Uuid::new_v4(),
        generation: 1,
        controller: Some(lease.clone()),
    };
    io.submit(
        &Effect::Fence(fence.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let receipt = wait(|| io.receipt(fence.fence_id).ok().flatten());
    assert!(matches!(receipt, terminal_io::Receipt::Fence(_)));
    (case, host, io, lease)
}
fn io_input(
    lease: &aq_session_protocol::ControllerLease,
    bytes: &[u8],
) -> aq_session_protocol::InputCommand {
    let now = io_now();
    aq_session_protocol::InputCommand {
        operation_id: Uuid::new_v4(),
        lease: lease.clone(),
        issued_at_ms: now,
        deadline_ms: now + 1500,
        bytes: bytes.to_vec(),
        escape_key: false,
    }
}
fn io_lines(host: &SessionHandle) -> Vec<String> {
    std::fs::read_to_string(host.directory().join("fixture-input.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
