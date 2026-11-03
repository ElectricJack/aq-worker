//! Observed sandbox API correlation is checked before any guardian or journal.
//! No physical tmux session, fake baseline, sleep, or server authority is used.
#[path = "support/sandbox_runner.rs"]
#[allow(dead_code)]
mod support;
use aq_session_host::{RenderedStartup, observed::*};
use aq_session_protocol::{ArtifactRef, Digest, TerminalIdentity, Text, canonical_digest};
use aq_worker::{
    config::WorkerConfig,
    native::runner::sandbox::{auth::RunnerHttp, *},
};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

struct Adapter;
impl aq_session_host::HarnessAdapter for Adapter {
    fn prepare(
        &self,
        _: &aq_session_protocol::LaunchManifest,
        _: &Path,
    ) -> anyhow::Result<aq_session_host::AdapterLaunch> {
        panic!("validation must precede physical launch")
    }
    fn readiness(
        &self,
        _: &aq_session_host::HostObservation,
    ) -> anyhow::Result<aq_session_host::HarnessReadiness> {
        unreachable!()
    }
}
impl ObservedHarnessAdapter for Adapter {
    fn observed_qualification(
        &self,
        _: &aq_session_protocol::LaunchManifest,
    ) -> anyhow::Result<ObservedAdapterQualification> {
        panic!("validation must precede physical launch")
    }
}
struct Transport(RunnerHttp);
impl RunnerTransport for Transport {
    fn authority_client(&self) -> Option<RunnerHttp> {
        Some(self.0.clone())
    }
    fn preflight(&mut self, _: &WorkerConfig) -> anyhow::Result<()> {
        anyhow::bail!("preflight sentinel")
    }
    fn renew<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        _: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
        _: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome> {
        unreachable!()
    }
    fn retain<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        _: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts> {
        unreachable!()
    }
    fn finished<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        _: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt> {
        unreachable!()
    }
}
fn artifact(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
#[tokio::test]
async fn observed_sandbox_checks_manifest_and_shared_permit_before_launch() {
    let mut f = support::Fixture::new(false, 5000);
    let payload = b"literal startup";
    let mut descriptor = artifact("descriptor", b"fixture descriptor");
    descriptor.id = Text::new(format!(
        "aq.startup-delivery/{}",
        descriptor.digest.as_str()
    ))
    .unwrap();
    f.frozen.manifest.modules.push(descriptor.clone());
    f.frozen.authority.grant.binding.launch_manifest_digest =
        canonical_digest(&f.frozen.manifest).unwrap();
    let files = private::RunnerFiles::create(&f.dir.path().join("private")).unwrap();
    files
        .write_secret("permit.json", &serde_json::to_vec(&f.permit).unwrap())
        .unwrap();
    let http = RunnerHttp::for_execution(
        "http://127.0.0.1:1",
        &files.root().join("permit.json"),
        &f.frozen,
    )
    .unwrap();
    let other = RunnerHttp::for_execution(
        "http://127.0.0.1:1",
        &files.root().join("permit.json"),
        &f.frozen,
    )
    .unwrap();
    let program = std::fs::canonicalize("/usr/bin/true").unwrap();
    let executable = artifact("true", &std::fs::read(&program).unwrap());
    let renderer = artifact("renderer", b"fixture");
    let pool = SealedElfPool::new(4 * 1024 * 1024, 2).unwrap();
    for case in 0..3 {
        let foreign_manifest = case == 0;
        let mut manifest = f.frozen.manifest.clone();
        if foreign_manifest {
            manifest.resources.wall_time_ms += 1;
        }
        let rendered = RenderedStartup::new(
            manifest,
            descriptor.clone(),
            artifact("payload", payload),
            payload.to_vec(),
        )
        .unwrap();
        let startup = ObservedStartup::new(
            rendered,
            TerminalIdentity {
                session: f.frozen.manifest.bindings.session.clone(),
                terminal_id: Uuid::new_v4(),
                epoch: 1,
            },
            pool.load_verified(&program, &executable).unwrap(),
            ObservationProfile::small_elf(renderer.clone(), 80, 24).unwrap(),
            support::now_ns() / 1_000_000 + 10_000,
        )
        .unwrap();
        let channels = Arc::new(
            channels::SandboxChannels::new(if case == 1 { &other } else { &http }).unwrap(),
        );
        let (_cancel, shutdown) = tokio::sync::watch::channel(false);
        let error = run(
            &f.worker,
            &f.frozen,
            f.permit.clone(),
            f.dir.path(),
            &f.local,
            SandboxStartup::observed(&startup, &Adapter, channels),
            &mut Transport(http.clone()),
            shutdown,
            RunOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains(if foreign_manifest {
                "observed startup manifest mismatch"
            } else if case == 1 {
                "another permit authority"
            } else {
                "preflight sentinel"
            }),
            "{error:#}"
        );
        assert!(!f.dir.path().join("sandbox-run").exists());
    }
}
