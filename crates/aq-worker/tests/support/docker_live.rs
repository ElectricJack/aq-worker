//! Shared harness for live docker-local qualification tests (I4a, calm-delta-18.9.4):
//! the controller-owned backend handle, the test credential lease and the
//! launch/node fixtures. Included by `#[path]`; it never runs on its own.
#![allow(dead_code)]
use aq_admission_sandbox::sandbox::{
    DelegatedPermit, HarnessVersion, Money, Registration, TemplateAdvertisement,
};
use aq_admission_sandbox::transport::WorkerIdentity;
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, SessionIdentity, Text};
use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use aq_worker::controller::*;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The owner's long-lived token: a private regular file, trimmed, never logged.
pub fn read_token(path: &Path) -> Zeroizing<Vec<u8>> {
    let meta = std::fs::metadata(path).expect("token file");
    assert!(meta.is_file(), "token path must be a regular file");
    assert_eq!(
        meta.permissions().mode() & 0o077,
        0,
        "token file must not be group/world readable"
    );
    let raw = Zeroizing::new(std::fs::read(path).expect("read token"));
    let token = Zeroizing::new(raw.trim_ascii().to_vec());
    assert!(!token.is_empty(), "token file is empty");
    token
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// The controller owns the backend; the test keeps a handle to read the
/// sandbox's outputs and observe the runner's exit.
#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<DockerLocalBackend>>);

impl PlacementBackend for Shared {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerLocal
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::PersistentDiskDuringAttempt]
    }
    async fn create(&mut self, r: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        self.0.lock().await.create(r).await
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> Result<(), BackendError> {
        self.0.lock().await.deliver_private_material(s, m).await
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        self.0.lock().await.start_runner(s).await
    }
    async fn inspect(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        self.0.lock().await.inspect(s).await
    }
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        self.0.lock().await.list_owned(node).await
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        self.0.lock().await.stop(s).await
    }
    async fn runner_exit(&mut self, s: &SandboxIdentity) -> Result<RunnerExit, BackendError> {
        self.0.lock().await.runner_exit(s).await
    }
    fn disk_limit(&self) -> DiskLimitStatus {
        self.0.try_lock().expect("backend idle").disk_limit()
    }
    async fn disk_usage(&mut self, node: Uuid) -> Result<Vec<DiskUsage>, BackendError> {
        self.0.lock().await.disk_usage(node).await
    }
    async fn measured_disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        self.0.lock().await.measured_disk_usage(preserved).await
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        self.0.lock().await.destroy(s, a).await
    }
}

/// Test credential lease: a preparation permit bound to the attempt, and the
/// subscription token as the private payload. No refresh, no write-back.
pub struct Lease {
    pub token: Zeroizing<Vec<u8>>,
    /// The reservation's hard deadline, which the permit must equal; set once
    /// the request is built (after the controller owns the provider).
    pub deadline: Arc<AtomicU64>,
}

impl PermitProvider for Lease {
    type Request = ();
    async fn register(&mut self, _: &Registration) -> anyhow::Result<()> {
        Ok(())
    }
    async fn issue(
        &mut self,
        _: &(),
        sandbox: &SandboxIdentity,
        manifest: &LaunchManifest,
    ) -> anyhow::Result<IssuedMaterial> {
        let deadline = self.deadline.load(Ordering::SeqCst);
        assert!(deadline > now_ms(), "lease deadline not set");
        let permit: DelegatedPermit = serde_json::from_value(json!({
            "schema_version": 1,
            "permit_id": Uuid::new_v4(),
            "credential": "opaque_secret_01234567890123456789012345",
            "binding": {
                "attempt_id": sandbox.owner.attempt_id,
                "grant_generation": manifest.bindings.assignment_generation,
                "sandbox_id": Uuid::new_v4(),
                "node": {"node_id": sandbox.owner.node_id, "boot_id": sandbox.owner.boot_id},
                "channel_id": Uuid::new_v4(),
            },
            "scope": "preparation",
            "issued_at_ms": now_ms() - 10,
            "expires_at_ms": deadline,
            "hard_deadline_ms": deadline,
        }))?;
        Ok(IssuedMaterial {
            permit,
            payload: PrivateMaterial::new(self.token.to_vec()),
        })
    }
}

pub fn launch(
    identity: &WorkerIdentity,
    digest: &TemplateDigest,
    harness: &str,
    cli: &str,
    run_timeout: Duration,
) -> LaunchRequest {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../aq-session-protocol/artifacts/fixtures.json"
    ))
    .unwrap();
    let mut m: LaunchManifest = serde_json::from_value(
        fixtures["valid_documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "launch")
            .unwrap()["body"]["manifest"]
            .clone(),
    )
    .unwrap();
    m.bindings.attempt_id = Uuid::new_v4();
    m.bindings.session = SessionIdentity::Remote {
        node_id: identity.node_id,
        boot_id: identity.boot_id,
        session_id: Uuid::new_v4(),
        attempt_id: m.bindings.attempt_id,
        assignment_generation: m.bindings.assignment_generation,
    };
    m.harness.harness = Text::new(harness).unwrap();
    m.harness.cli_version = Text::new(cli).unwrap();
    m.harness.environment.digest =
        serde_json::from_value(json!(digest.as_str().strip_prefix("sha256:").unwrap())).unwrap();
    m.resources.cpu_millis = 2000;
    m.resources.memory_bytes = 4 << 30;
    m.resources.process_limit = 512;
    m.resources.wall_time_ms = run_timeout.as_millis() as u64;
    m.resources.stop_grace_ms = 5_000;
    LaunchRequest {
        manifest: m,
        hard_deadline_ms: now_ms() + run_timeout.as_millis() as u64 + 60_000,
        disk_bytes: 4 << 30,
        required_capabilities: vec![Capability::PersistentDiskDuringAttempt],
    }
}

pub fn node(digest: &TemplateDigest, harness: &str, cli: &str) -> NodeConfig {
    NodeConfig {
        preserve: Default::default(),
        runtime: ArtifactRef {
            id: Text::new("runtime").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"runtime"),
        },
        capabilities: vec![],
        features: vec![],
        templates: vec![TemplateAdvertisement {
            digest: serde_json::from_value(json!(digest.as_str().strip_prefix("sha256:").unwrap()))
                .unwrap(),
            harnesses: vec![HarnessVersion {
                harness: harness.into(),
                version: cli.into(),
            }],
            estimated_cold_start_ms: 30_000,
            estimated_cold_start_cost: Money {
                currency: "USD".into(),
                micros: 0,
            },
        }],
        max_concurrent_sandboxes: 1,
        spend_ceiling: None,
        capacity: CapacityLimits {
            memory_bytes: 8 << 30,
            cpu_millis: 4000,
            disk_bytes: 16 << 30,
            max_preparing: 1,
        },
        recovery: RecoveryConfig::default(),
    }
}

pub async fn read(shared: &Shared, sandbox: &SandboxIdentity, path: &str) -> Option<Vec<u8>> {
    shared.0.lock().await.read_file(sandbox, path).await.ok()
}
