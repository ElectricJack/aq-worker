use aq_admission_sandbox::{
    sandbox::{HarnessVersion, Money, TemplateAdvertisement},
    transport::WorkerIdentity,
};
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, SessionIdentity, Text};
use aq_worker::backend::conformance::{self, BackendFixture};
use aq_worker::backend::{boat::*, *};
use aq_worker::controller::*;
use axum::http::Method;
use serde_json::json;
use uuid::Uuid;
#[path = "support/boat_double.rs"]
mod boat_double;
use boat_double::BoatDouble;
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn digest() -> TemplateDigest {
    TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap()
}
fn config(api: &BoatDouble, dir: &std::path::Path) -> BoatConfig {
    serde_json::from_value(json!({
    "api_url":api.url,"node_id":Uuid::from_u128(1),"attempt_org":"team_attempts","template_org":"team_builds",
    "journal_path":dir.join("boat.json"),"key_file":dir.join("key"),"api_timeout_ms":1000,"poll_timeout_ms":25,"poll_interval_ms":1,
    "templates":[{"image":format!("registry.example/template@{}",digest().as_str()),"digest":digest(),"saved_snapshot":"fixture-template","snapshot_id":"template-artifact","scan_policy":"aq-credential-scan-v1"}]
})).unwrap()
}
fn request(case: u128) -> CreateRequest {
    CreateRequest::new(
        Ownership {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(case + 10),
        },
        digest(),
        now() + 60_000,
        vec![Capability::VmIsolation],
    )
    .with_resources(ResourceLimits {
        cpu_millis: 1000,
        memory_bytes: 1024 * 1024 * 1024,
        pids: 64,
        disk_bytes: None,
    })
}
async fn backend(api: &BoatDouble, dir: &std::path::Path) -> BoatBackend {
    BoatBackend::connect(
        config(api, dir),
        BoatKey::new("fixture-node-key".into()).unwrap(),
    )
    .unwrap()
}
struct Fixture {
    backend: BoatBackend,
    api: BoatDouble,
    _dir: tempfile::TempDir,
    retained: Vec<Vec<u8>>,
}
impl Fixture {
    async fn new() -> Self {
        let api = BoatDouble::start().await;
        let dir = tempfile::tempdir().unwrap();
        let backend = backend(&api, dir.path()).await;
        Self {
            backend,
            api,
            _dir: dir,
            retained: vec![],
        }
    }
}
impl BackendFixture for Fixture {
    type Backend = BoatBackend;
    fn backend(&mut self) -> &mut BoatBackend {
        &mut self.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        request(case)
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        match fault {
            Fault::CrashAfterCreate => self.api.fault(Method::POST, "/sandboxes", 0, true),
            Fault::DestroyFailure => {
                let id = self
                    .api
                    .data
                    .lock()
                    .unwrap()
                    .sandboxes
                    .keys()
                    .next()
                    .unwrap()
                    .clone();
                self.api
                    .fault(Method::DELETE, &format!("/sandboxes/{id}"), 503, false)
            }
            _ => panic!("unused conformance fault"),
        }
    }
    async fn observe_fixture(&mut self, s: &SandboxIdentity) {
        assert_eq!(
            self.backend.read_work_file(s, "result").await.unwrap(),
            b"fixture-complete\n"
        );
    }
    async fn verify_and_retain(&mut self, s: &SandboxIdentity) -> DestroyAuthorization {
        let b = self.backend.read_work_file(s, "result").await.unwrap();
        assert_eq!(b, b"fixture-complete\n");
        self.retained.push(b);
        DestroyAuthorization::retained(s.clone(), "fixture-receipt").unwrap()
    }
}
#[tokio::test]
async fn boat_http_conformance() {
    conformance::run_all(&mut Fixture::new().await).await;
}
#[tokio::test]
async fn boat_policy_deadlines_and_secret_delivery() {
    let mut f = Fixture::new().await;
    let r = request(30);
    let s = f.backend.create(&r).await.unwrap();
    assert_eq!(
        f.backend.capabilities(),
        vec![
            Capability::VmIsolation,
            Capability::SuspendSnapshot,
            Capability::PersistentDiskDuringAttempt
        ]
    );
    f.backend
        .deliver_private_material(&s, f.material())
        .await
        .unwrap();
    f.backend.start_runner(&s).await.unwrap();
    let reqs = f.api.data.lock().unwrap().requests.clone();
    let body = &reqs
        .iter()
        .find(|(m, p, _)| *m == Method::POST && p.ends_with("/sandboxes"))
        .unwrap()
        .2;
    assert_eq!(body["type"], "small");
    assert!((55..=66).contains(&body["ttlSeconds"].as_u64().unwrap()));
    let cmd = reqs
        .iter()
        .filter(|(_, p, _)| p.ends_with("/commands"))
        .map(|(_, _, b)| b["command"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for policy in [
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pids-limit",
        "--memory",
        "--cpus",
        "--network=bridge",
    ] {
        assert!(cmd.contains(policy), "{policy}");
    }
    assert!(!cmd.contains("docker.sock"));
    assert!(!cmd.contains("fixture-private-material"));
    assert!(!format!("{:?}", BoatKey::new("sensitive".into()).unwrap()).contains("sensitive"));
    assert!(
        f.backend
            .extend_auto_stop(&s, r.hard_deadline_ms + 1)
            .await
            .is_err()
    );
    assert!(f.backend.archive(&s).await.is_confirmed());
}
#[tokio::test]
async fn boat_uncertain_archive_and_delete_never_confirm() {
    let mut f = Fixture::new().await;
    let s = f.backend.create(&request(40)).await.unwrap();
    f.api.data.lock().unwrap().lie_stop = true;
    assert!(!f.backend.stop(&s).await.is_confirmed());
    f.api.data.lock().unwrap().lie_stop = false;
    assert!(f.backend.stop(&s).await.is_confirmed());
    let auth = DestroyAuthorization::retained(s.clone(), "receipt").unwrap();
    f.api.data.lock().unwrap().lie_delete = true;
    assert!(!f.backend.destroy(&s, &auth).await.is_confirmed());
    f.api.data.lock().unwrap().lie_delete = false;
    f.api.data.lock().unwrap().snapshot_left = true;
    assert!(!f.backend.destroy(&s, &auth).await.is_confirmed());
    f.api.data.lock().unwrap().snapshot_left = false;
    f.api.data.lock().unwrap().lie_purge = true;
    assert!(
        !f.backend.destroy(&s, &auth).await.is_confirmed(),
        "HTTP 404 does not prove storage purge completed"
    );
    f.api.data.lock().unwrap().lie_purge = false;
    assert!(f.backend.destroy(&s, &auth).await.is_confirmed());
}
#[tokio::test]
async fn boat_mapping_survives_restart_and_rejects_changed_request() {
    let api = BoatDouble::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut b = backend(&api, dir.path()).await;
    let r = request(50);
    api.fault(Method::POST, "/sandboxes", 0, true);
    assert!(b.create(&r).await.is_err());
    drop(b);
    let mut b = backend(&api, dir.path()).await;
    let s = b.create(&r).await.unwrap();
    assert_eq!(b.list_owned(r.owner.node_id).await.unwrap().len(), 1);
    let mut changed = r.clone();
    changed.owner.boot_id = Uuid::new_v4();
    assert_eq!(
        b.create(&changed).await.unwrap_err(),
        BackendError::Conflict
    );
    assert!(b.stop(&s).await.is_confirmed());
}
#[tokio::test]
async fn boat_rejects_unsafe_requests_and_mutated_snapshot() {
    let mut f = Fixture::new().await;
    let mut r = request(60);
    r.required_capabilities.push(Capability::NestedDocker);
    assert!(matches!(
        f.backend.create(&r).await,
        Err(BackendError::UnsupportedCapability(
            Capability::NestedDocker
        ))
    ));
    r = request(61);
    r.resources.as_mut().unwrap().cpu_millis = 9000;
    assert!(f.backend.create(&r).await.is_err());
    r = request(62).with_mount_aliases(vec!["host".into()]);
    assert!(f.backend.create(&r).await.is_err());
    f.api.data.lock().unwrap().snapshot_id = "replaced".into();
    assert!(f.backend.create(&request(63)).await.is_err());
    assert_eq!(f.api.running(), 0);
}
struct Provider;
impl PermitProvider for Provider {
    type Request = ();
    async fn register(
        &mut self,
        _: &aq_admission_sandbox::sandbox::Registration,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn issue(
        &mut self,
        _: &(),
        _: &SandboxIdentity,
        _: &LaunchManifest,
    ) -> anyhow::Result<IssuedMaterial> {
        anyhow::bail!("not reached")
    }
}
fn launch(identity: &WorkerIdentity) -> LaunchRequest {
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../aq-session-protocol/artifacts/fixtures.json"
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
    m.bindings.attempt_id = Uuid::from_u128(100);
    m.bindings.session = SessionIdentity::Remote {
        node_id: identity.node_id,
        boot_id: identity.boot_id,
        session_id: Uuid::from_u128(101),
        attempt_id: m.bindings.attempt_id,
        assignment_generation: m.bindings.assignment_generation,
    };
    m.resources.cpu_millis = 1000;
    m.resources.memory_bytes = 64 * 1024 * 1024;
    LaunchRequest {
        manifest: m,
        hard_deadline_ms: now() + 60_000,
        disk_bytes: 1,
        required_capabilities: vec![Capability::VmIsolation],
    }
}
fn node(r: &LaunchRequest) -> NodeConfig {
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
            digest: r.manifest.harness.environment.digest.clone(),
            harnesses: vec![HarnessVersion {
                harness: r.manifest.harness.harness.as_str().into(),
                version: r.manifest.harness.cli_version.as_str().into(),
            }],
            estimated_cold_start_ms: 1000,
            estimated_cold_start_cost: Money {
                currency: "USD".into(),
                micros: 10,
            },
        }],
        max_concurrent_sandboxes: 2,
        spend_ceiling: None,
        capacity: CapacityLimits {
            memory_bytes: 1 << 30,
            cpu_millis: 4000,
            disk_bytes: 1 << 30,
            max_preparing: 2,
        },
        recovery: RecoveryConfig::default(),
    }
}
#[tokio::test]
async fn boat_unavailable_refuses_offers_without_orphans() {
    for (status, after) in [
        (503, false),
        (429, false),
        (409, false),
        (503, true),
        (429, true),
        (409, true),
    ] {
        let api = BoatDouble::start().await;
        let dir = tempfile::tempdir().unwrap();
        let placeholder = WorkerIdentity {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
        };
        let r = launch(&placeholder);
        let mut cfg = config(&api, dir.path());
        cfg.templates[0].digest = TemplateDigest::parse(format!(
            "sha256:{}",
            r.manifest.harness.environment.digest.as_str()
        ))
        .unwrap();
        cfg.templates[0].image = format!(
            "registry.example/template@{}",
            cfg.templates[0].digest.as_str()
        );
        let b =
            BoatBackend::connect(cfg, BoatKey::new("fixture-node-key".into()).unwrap()).unwrap();
        let mut c = Controller::open(
            &dir.path().join("controller"),
            placeholder.node_id,
            node(&r),
            b,
            Provider,
        )
        .unwrap();
        c.register().await.unwrap();
        let r = launch(c.identity());
        let deadline = r.hard_deadline_ms;
        let id = c.reserve(r).unwrap();
        c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
        api.fault(Method::POST, "/sandboxes", status, after);
        let error = c.launch(id, &()).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<RefusalReason>(),
            Some(&RefusalReason::BackendUnavailable)
        );
        assert_eq!(
            c.inventory().refusal,
            Some(RefusalReason::BackendUnavailable)
        );
        c.sweep_at(deadline + 1).await.unwrap();
        c.reconcile_at(deadline + 1).await.unwrap();
        assert_eq!(api.running(), 0, "status={status}, after={after}");
    }
}
#[tokio::test]
async fn sweep_ignores_sandboxes_without_node_prefix() {
    let api = BoatDouble::start().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&api, dir.path()).await;
    let identity = WorkerIdentity {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
    };
    let r = launch(&identity);
    api.insert("bx_foreign1", "someones-server", "team_attempts");
    api.insert("bx_foreign2", &request(70).name, "team_other");
    let mut foreign = request(71);
    foreign.owner.node_id = Uuid::from_u128(9);
    api.insert(
        "bx_foreign3",
        &foreign.owner.deterministic_name(),
        "team_attempts",
    );
    api.insert(
        "bx_own",
        "aq-00000000000000000000000000000001-00000000000000000000000000000099",
        "team_attempts",
    );
    api.data.lock().unwrap().page = true;
    let mut c = Controller::open(
        &dir.path().join("controller"),
        identity.node_id,
        node(&r),
        b,
        Provider,
    )
    .unwrap();
    c.sweep_at(now()).await.unwrap();
    let d = api.data.lock().unwrap();
    assert_eq!(d.sandboxes["bx_own"]["state"], "archived");
    for id in ["bx_foreign1", "bx_foreign2", "bx_foreign3"] {
        assert_eq!(d.sandboxes[id]["state"], "running");
    }
    assert_eq!(
        d.requests
            .iter()
            .filter(|(m, p, _)| *m == Method::POST && p.ends_with("/stop"))
            .count(),
        1
    );
}
#[tokio::test]
async fn boat_restart_keeps_delete_operation_and_journal_failure_fences_effects() {
    let api = BoatDouble::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut b = backend(&api, dir.path()).await;
    let r = request(80);
    let s = b.create(&r).await.unwrap();
    let auth = DestroyAuthorization::retained(s.clone(), "receipt").unwrap();
    api.data.lock().unwrap().lie_purge = true;
    assert!(!b.destroy(&s, &auth).await.is_confirmed());
    drop(b);
    let mut b = backend(&api, dir.path()).await;
    api.data.lock().unwrap().lie_purge = false;
    assert!(b.destroy(&s, &auth).await.is_confirmed());
    // Replacing the journal with a directory makes atomic persistence fail.
    std::fs::remove_file(dir.path().join("boat.json")).unwrap();
    std::fs::create_dir(dir.path().join("boat.json")).unwrap();
    let count = api.data.lock().unwrap().requests.len();
    assert!(b.create(&request(81)).await.is_err());
    assert!(b.create(&request(81)).await.is_err());
    assert_eq!(
        api.data.lock().unwrap().requests.len(),
        count,
        "failed durable intent must fence all later effects"
    );
}
#[tokio::test]
async fn boat_vm_sizes_and_ttl_updates_are_bounded() {
    let mut f = Fixture::new().await;
    for (case, cpu, mem, size) in [
        (90, 2000, 4u64 << 30, "small"),
        (91, 4000, 8u64 << 30, "default"),
        (92, 8000, 16u64 << 30, "large"),
    ] {
        let mut r = request(case);
        r.resources = Some(ResourceLimits {
            cpu_millis: cpu,
            memory_bytes: mem,
            pids: 128,
            disk_bytes: None,
        });
        let s = f.backend.create(&r).await.unwrap();
        f.backend
            .extend_auto_stop(&s, r.hard_deadline_ms)
            .await
            .unwrap();
        {
            let d = f.api.data.lock().unwrap();
            let body = &d
                .requests
                .iter()
                .rev()
                .find(|(m, p, _)| *m == Method::POST && p.ends_with("/sandboxes"))
                .unwrap()
                .2;
            assert_eq!(body["type"], size);
        }
        assert!(f.backend.stop(&s).await.is_confirmed());
    }
    let mut expired = request(93);
    expired.hard_deadline_ms = now() - 1;
    assert!(f.backend.create(&expired).await.is_err());
    let d = f.api.data.lock().unwrap();
    for (method, _, body) in &d.requests {
        if *method == Method::PATCH {
            assert!((1..=65).contains(&body["ttlSeconds"].as_u64().unwrap()));
        }
    }
}
#[tokio::test]
async fn boat_run_configuration_is_exclusive_and_key_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let api = BoatDouble::start().await;
    let dir = tempfile::tempdir().unwrap();
    let b = config(&api, dir.path());
    let r = launch(&WorkerIdentity {
        node_id: b.node_id,
        boot_id: Uuid::from_u128(2),
    });
    let mut n = node(&r);
    n.templates[0].digest =
        serde_json::from_value(json!(digest().as_str().trim_start_matches("sha256:"))).unwrap();
    let boat = json!({"api_url":b.api_url,"node_id":b.node_id,"attempt_org":b.attempt_org,"template_org":b.template_org,"journal_path":b.journal_path,"key_file":b.key_file,"templates":b.templates});
    let mut value = json!({"state_dir":dir.path().join("controller"),"node":n,"boat":boat});
    let path = dir.path().join("node.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(RunConfig::load(&path).is_ok());
    value["docker_local"] = json!({"socket":"/tmp/dedicated-docker.sock"});
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(RunConfig::load(&path).is_err());
    value.as_object_mut().unwrap().remove("boat");
    value.as_object_mut().unwrap().remove("docker_local");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(RunConfig::load(&path).is_err());
    let key = dir.path().join("key");
    std::fs::write(&key, "fixture-node-key").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(BoatKey::load(&key).is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(BoatKey::load(&key).is_ok());
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&key, &alias).unwrap();
    assert!(BoatKey::load(&alias).is_err());
}
#[tokio::test]
async fn boat_orphan_retention_authority_can_delete_after_restart_without_inventing_boot() {
    let api = BoatDouble::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut b = backend(&api, dir.path()).await;
    api.insert("bx_orphan", &request(200).name, "team_attempts");
    let e = b
        .list_owned(Uuid::from_u128(1))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(e.sandbox.owner.boot_id.is_nil());
    assert!(b.stop(&e.sandbox).await.is_confirmed());
    assert_eq!(
        b.inspect(&e.sandbox).await.unwrap().state,
        SandboxState::Archived
    );
    let a = DestroyAuthorization::operator_released(e.sandbox.clone(), "retention-reviewed-orphan")
        .unwrap();
    api.data.lock().unwrap().lie_purge = true;
    assert!(!b.destroy(&e.sandbox, &a).await.is_confirmed());
    drop(b);
    let mut b = backend(&api, dir.path()).await;
    api.data.lock().unwrap().lie_purge = false;
    assert!(b.destroy(&e.sandbox, &a).await.is_confirmed());
    assert_eq!(
        b.inspect(&e.sandbox).await.unwrap().state,
        SandboxState::Destroyed
    );
    assert!(b.list_owned(Uuid::from_u128(1)).await.unwrap().is_empty());
}
#[tokio::test]
#[ignore = "requires approved dedicated BOAT_LIVE_CONFIG and BOAT_API_KEY; runs paid VMs"]
async fn boat_live_conformance() {
    if std::env::var_os("BOAT_LIVE_CONFIG").is_none() || std::env::var_os("BOAT_API_KEY").is_none()
    {
        eprintln!(
            "NOT RUN: live Boat qualification requires both BOAT_LIVE_CONFIG and BOAT_API_KEY"
        );
        return;
    }
    // The live fixture runs the same lifecycle scenarios with one-shot faults at
    // the client boundary, and cleans every prefixed sandbox on panic or success.
    live::run().await;
}
#[path = "support/boat_live.rs"]
mod live;
