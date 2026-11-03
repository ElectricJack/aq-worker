use anyhow::Result;
use aq_admission_sandbox::{
    control::DeliveryStamp,
    sandbox::{DelegatedPermit, RunnerRenewalRequest},
};
use aq_session_host::*;
use aq_session_protocol::{LaunchManifest, canonical_digest};
use aq_worker::{config::WorkerConfig, native::runner::sandbox::*};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    time::Duration,
};
use tempfile::TempDir;
use uuid::Uuid;

pub struct Fixture {
    pub dir: TempDir,
    pub frozen: FrozenLaunch,
    pub permit: DelegatedPermit,
    pub worker: WorkerConfig,
    pub local: LocalEnvironment,
    pub adapter: Adapter,
}
pub struct Adapter {
    pub hold: bool,
    pub descendant: bool,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, manifest: &LaunchManifest, workspace: &Path) -> Result<AdapterLaunch> {
        let mut environment = BTreeMap::new();
        if self.hold {
            environment.insert("AQ_FIXTURE_HOLD".into(), "1".into());
        }
        if self.descendant {
            environment.insert("AQ_FIXTURE_DESCENDANT".into(), "1".into());
            environment.insert("AQ_FIXTURE_IGNORE_TERM".into(), "1".into());
        }
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: canonical_digest(manifest)?,
            adapter: manifest.harness.adapter.clone(),
            cli_version: manifest.harness.cli_version.as_str().into(),
            environment: manifest.harness.environment.clone(),
            capabilities: manifest.harness.required_capabilities.clone(),
            command: LocalCommand {
                executable: env!("CARGO_BIN_EXE_aq-fixture-agent").into(),
                arguments: vec!["--launch-dir".into(), workspace.to_str().unwrap().into()],
                environment,
            },
        })
    }
    fn readiness(&self, observation: &HostObservation) -> Result<HarnessReadiness> {
        Ok(if observation.phase == HostPhase::Stopped {
            HarnessReadiness::Exited
        } else {
            HarnessReadiness::Ready
        })
    }
}
pub fn now_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
impl Fixture {
    pub fn new(hold: bool, ttl_ms: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join("input.json"),
            r#"{"output":"sandbox fixture output"}"#,
        )
        .unwrap();
        let context = dir.path().join("context");
        fs::write(&context, b"fixture").unwrap();
        let mut manifest: LaunchManifest = serde_json::from_slice(include_bytes!(
            "../../../aq-session-host/tests/fixtures/manifest.json"
        ))
        .unwrap();
        let vectors: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../../protocol/aq-admission/artifacts/sandbox-v1.fixtures.json"
        ))
        .unwrap();
        let mut request: RunnerRenewalRequest = serde_json::from_value(
            vectors
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["name"] == "phase_v2_renewal")
                .unwrap()["case"]["request"]
                .clone(),
        )
        .unwrap();
        manifest.bindings.session = request.authority.grant.binding.session.clone();
        manifest.bindings.assignment_generation = 2;
        manifest.terminal.input_enabled = false;
        manifest.resources.wall_time_ms = 20_000;
        request.authority.grant.binding.graph_access_profile_digest =
            manifest.bindings.graph_access_profile.digest.clone();
        request.authority.grant.binding.launch_manifest_digest =
            canonical_digest(&manifest).unwrap();
        request.authority.grant.expires_at_ms = 1000 + ttl_ms;
        request.authority.hard_deadline_ms = 21_000;
        request.permit.expires_at_ms = 1000 + ttl_ms;
        request.permit.hard_deadline_ms = 21_000;
        let now = now_ns();
        let meta = fs::metadata(dir.path()).unwrap();
        let frozen = FrozenLaunch { schema_version: 1, launch_id: Uuid::new_v4(), manifest: manifest.clone(), authority: request.authority, state_dir: dir.path().canonicalize().unwrap(), state_device: meta.dev(), state_inode: meta.ino(), kernel_boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim().into(), delivery: serde_json::from_value::<DeliveryStamp>(serde_json::json!({"nonce":Uuid::new_v4(),"clock":{"server_time_ms":1000,"expires_at_ms":1000+ttl_ms,"valid_for_ms":ttl_ms}})).unwrap(), sent_boot_ns:now, received_boot_ns:now, revision:1 };
        let reference = manifest.harness.adapter.clone();
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(reference.clone()),
            workspace,
            helper_executable: env!("CARGO_BIN_EXE_aq-fixture-agent").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: String::from_utf8(
                std::process::Command::new("/usr/bin/tmux")
                    .arg("-V")
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .into(),
            credential_profiles: vec![reference.clone()],
            context_material: vec![ContextMaterial {
                artifact: reference,
                path: context,
            }],
        };
        let worker = WorkerConfig {
            server_url: "https://fixture.invalid".into(),
            node_id: request.permit.binding.node.node_id,
            credential: "fixture-worker-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        Self {
            dir,
            frozen,
            permit: request.permit,
            worker,
            local,
            adapter: Adapter {
                hold,
                descendant: hold,
            },
        }
    }
    pub async fn run(&self, transport: &mut impl RunnerTransport) -> Result<RunOutcome> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        run(
            &self.worker,
            &self.frozen,
            self.permit.clone(),
            self.dir.path(),
            &self.local,
            &self.adapter,
            transport,
            rx,
            RunOptions {
                renewal_interval: Duration::from_millis(500),
            },
        )
        .await
    }
}
#[derive(Clone, Copy)]
pub enum Mode {
    Complete,
    MissingSource,
    MissingResult,
    Refuse,
    Stall,
}
pub struct Central {
    pub dir: TempDir,
    pub mode: Mode,
    pub renewals: usize,
    pub finishes: usize,
    pub retentions: usize,
}
impl Central {
    pub fn new(mode: Mode) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            mode,
            renewals: 0,
            finishes: 0,
            retentions: 0,
        }
    }
}
impl RunnerTransport for Central {
    fn preflight(&mut self, worker: &WorkerConfig) -> Result<()> {
        assert_eq!(worker.credential, "fixture-worker-secret");
        Ok(())
    }
    fn renew<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        _: &'a RunnerRenewalRequest,
        _: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome> {
        self.renewals += 1;
        Box::pin(async move {
            match self.mode {
                Mode::Stall => std::future::pending().await,
                _ => Ok(RenewalOutcome::Refused),
            }
        })
    }
    fn retain<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        stopped: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts> {
        self.retentions += 1;
        Box::pin(async move {
            let source = fs::read(stopped.workspace().join("output.txt"))?;
            let source_path = self.dir.path().join("source");
            let result_path = self.dir.path().join("result");
            durable_new(&source_path, &source)?;
            durable_new(&result_path, stopped.evidence())?;
            let receipt = |bytes: &[u8]| CentralReceipt {
                receipt_id: Uuid::new_v4(),
                attempt: stopped.key().clone(),
                artifact_digest: aq_session_protocol::Digest::of(bytes),
            };
            Ok(RetentionReceipts {
                source: (!matches!(self.mode, Mode::MissingSource)).then(|| receipt(&source)),
                result: (!matches!(self.mode, Mode::MissingResult))
                    .then(|| receipt(stopped.evidence())),
            })
        })
    }
    fn finished<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        retained: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt> {
        self.finishes += 1;
        Box::pin(async move {
            assert!(
                self.dir.path().join("source").exists() && self.dir.path().join("result").exists()
            );
            assert!(retained.receipts().source.is_some() && retained.receipts().result.is_some());
            Ok(FinishedReceipt {
                receipt_id: Uuid::new_v4(),
                attempt: retained.stopped().key().clone(),
            })
        })
    }
}

#[derive(Clone, Copy)]
pub enum RenewalMode {
    Renew,
    Replay,
    BadNonce,
    Awaiting,
}
pub struct Renewing {
    pub central: Central,
    pub mode: RenewalMode,
    pub origin_ns: u64,
    pub previous: Option<aq_admission_sandbox::sandbox::RunnerRenewalResponse>,
}
impl Renewing {
    pub fn new(f: &Fixture, mode: RenewalMode) -> Self {
        Self {
            central: Central::new(Mode::Complete),
            mode,
            origin_ns: f.frozen.sent_boot_ns,
            previous: None,
        }
    }
}
impl RunnerTransport for Renewing {
    fn preflight(&mut self, worker: &WorkerConfig) -> Result<()> {
        self.central.preflight(worker)
    }
    fn renew<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        request: &'a RunnerRenewalRequest,
        nonce: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome> {
        use aq_admission_sandbox::sandbox::{
            GrantPhase, PermitContext, RenewalReply, RunnerRenewalResponse,
        };
        self.central.renewals += 1;
        Box::pin(async move {
            let now = 1000 + (now_ns() - self.origin_ns) / 1_000_000;
            let mut permit = request.permit.clone();
            permit.issued_at_ms = now;
            permit.expires_at_ms = (now + 3000)
                .max(request.permit.expires_at_ms + 1000)
                .min(permit.hard_deadline_ms);
            let context = PermitContext {
                binding: permit.binding.clone(),
                phase: GrantPhase::Granted,
                grant_expires_at_ms: request.permit.expires_at_ms,
                hard_deadline_ms: permit.hard_deadline_ms,
            };
            let mut response = RunnerRenewalResponse {
                schema_version: 1,
                revision: request.revision + 1,
                reply: RenewalReply::Renewed {
                    operation_id: request.operation_id,
                    permit: permit.clone(),
                },
                delivery: Some(serde_json::from_value(
                    serde_json::json!({"nonce":nonce,"clock":{"server_time_ms":now,"expires_at_ms":permit.expires_at_ms,"valid_for_ms":permit.expires_at_ms-now}}),
                )?),
            };
            match self.mode {
                RenewalMode::BadNonce => response.delivery.as_mut().unwrap().nonce = Uuid::new_v4(),
                RenewalMode::Replay => {
                    if let Some(previous) = &self.previous {
                        response = previous.clone();
                    }
                }
                RenewalMode::Awaiting => {
                    response.revision = request.revision;
                    response.reply = RenewalReply::AwaitingNodeReadoption {
                        operation_id: request.operation_id,
                        binding: request.permit.binding.clone(),
                    };
                    response.delivery = None;
                }
                _ => {}
            }
            self.previous = Some(response.clone());
            Ok(RenewalOutcome::Delivered(Box::new(RenewalDelivery {
                response,
                context,
                server_now_ms: now,
            })))
        })
    }
    fn retain<'a>(
        &'a mut self,
        worker: &'a WorkerConfig,
        stopped: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts> {
        self.central.retain(worker, stopped)
    }
    fn finished<'a>(
        &'a mut self,
        worker: &'a WorkerConfig,
        retained: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt> {
        self.central.finished(worker, retained)
    }
}
