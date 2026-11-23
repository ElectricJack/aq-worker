use anyhow::Result;
use aq_admission_sandbox::{
    sandbox::{Money, Registration},
    transport::WorkerIdentity,
};
use aq_session_protocol::{LaunchManifest, Text};
use aq_worker::{backend::*, controller::*};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
use uuid::Uuid;
#[path = "../support/controller_config.rs"]
mod fixture;
use fixture::{config, reason, request, unavailable};
struct Provider;
impl PermitProvider for Provider {
    type Request = ();
    async fn register(&mut self, registration: &Registration) -> Result<()> {
        registration.validate()?;
        Ok(())
    }
    async fn issue(
        &mut self,
        _: &(),
        _: &SandboxIdentity,
        _: &LaunchManifest,
    ) -> Result<IssuedMaterial> {
        anyhow::bail!("unused")
    }
}
fn open(dir: &tempfile::TempDir, config: NodeConfig) -> Controller<FakeBackend, Provider> {
    try_open(dir, config).unwrap()
}
fn try_open(
    dir: &tempfile::TempDir,
    config: NodeConfig,
) -> Result<Controller<FakeBackend, Provider>> {
    Controller::open(
        &dir.path().join("node"),
        Uuid::from_u128(1),
        config,
        FakeBackend::default(),
        Provider,
    )
}
#[tokio::test]
async fn capacity_refuses_offer_at_sandbox_ceiling_with_typed_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    c.register().await.unwrap();
    c.reserve(request(c.identity(), 10)).unwrap();
    assert_eq!(
        reason(c.reserve(request(c.identity(), 11)).unwrap_err()),
        RefusalReason::SandboxCeiling
    );
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    assert_eq!(c.inventory().refusal, Some(RefusalReason::SandboxCeiling));
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("node/journal.json")).unwrap()).unwrap();
    assert_eq!(saved["state"]["last_refusal"], "sandbox_ceiling");
}
#[tokio::test]
async fn capacity_checks_spend_resources_environment_and_drain() {
    for (which, want) in [
        (0, RefusalReason::SpendCeiling),
        (1, RefusalReason::CapacityChanged),
        (2, RefusalReason::DiskReserve),
        (3, RefusalReason::EnvironmentUnavailable),
        (4, RefusalReason::PreparationBacklog),
        (5, RefusalReason::Draining),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.max_concurrent_sandboxes = 4;
        match which {
            0 => {
                cfg.spend_ceiling = Some(Money {
                    currency: "USD".into(),
                    micros: 9,
                })
            }
            1 => cfg.capacity.memory_bytes = 1,
            2 => cfg.capacity.disk_bytes = 1,
            4 => cfg.capacity.max_preparing = 1,
            _ => {}
        }
        let mut c = open(&dir, cfg);
        c.register().await.unwrap();
        let mut r = request(c.identity(), 10);
        if which == 2 {
            r.disk_bytes = 2;
        }
        if which == 3 {
            r.manifest.harness.cli_version = Text::new("missing").unwrap();
        }
        if which == 4 {
            c.reserve(request(c.identity(), 11)).unwrap();
        }
        if which == 5 {
            c.set_draining(true).unwrap();
        }
        assert_eq!(reason(c.reserve(r).unwrap_err()), want);
    }
}
#[tokio::test]
async fn reservation_is_durable_before_acceptance_and_timeout_holds_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    c.register().await.unwrap();
    let id = c.reserve(request(c.identity(), 10)).unwrap();
    let result: Result<()> = c
        .accept_with(id, |operation| async move {
            let v: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.path().join("node/journal.json")).unwrap())
                    .unwrap();
            assert_eq!(
                v["state"]["attempts"][id.to_string()]["phase"],
                "accept_intent"
            );
            assert_eq!(
                v["state"]["attempts"][id.to_string()]["accept_operation_id"],
                operation.to_string()
            );
            anyhow::bail!("lost response")
        })
        .await;
    assert!(result.is_err());
    assert_eq!(c.inventory().reserved_sandboxes, 1);
}
#[tokio::test]
async fn startup_rotates_boot_and_holds_old_reservations() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    c.register().await.unwrap();
    let old = c.identity().boot_id;
    c.reserve(request(c.identity(), 10)).unwrap();
    drop(c);
    let mut c = open(&dir, config());
    assert_ne!(old, c.identity().boot_id);
    assert_eq!(c.inventory().reserved_sandboxes, 1);
    c.register().await.unwrap();
    assert_eq!(
        reason(c.reserve(request(c.identity(), 11)).unwrap_err()),
        RefusalReason::GrantDenied
    );
}
#[test]
fn startup_rejects_second_owner_bad_version_symlink_and_missing_state() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, config());
    assert!(try_open(&dir, config()).is_err());
    drop(c);
    let path = dir.path().join("node/journal.json");
    let original = fs::read(&path).unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&original).unwrap();
    v["state"]["format"] = 999.into();
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(try_open(&dir, config()).is_err());
    fs::remove_file(&path).unwrap();
    symlink(dir.path().join("elsewhere"), &path).unwrap();
    assert!(try_open(&dir, config()).is_err());
    fs::remove_file(path).unwrap();
    assert!(try_open(&dir, config()).is_err());
    assert_eq!(
        fs::metadata(dir.path().join("node"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
async fn unregistered_and_wrong_boot_offers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    assert_eq!(
        reason(c.reserve(request(c.identity(), 10)).unwrap_err()),
        RefusalReason::GrantDenied
    );
    c.register().await.unwrap();
    let foreign = WorkerIdentity {
        node_id: c.identity().node_id,
        boot_id: Uuid::new_v4(),
    };
    assert_eq!(
        reason(c.reserve(request(&foreign, 10)).unwrap_err()),
        RefusalReason::GrantDenied
    );
    assert!(c.state().attempts.is_empty());
}
#[tokio::test]
async fn root_replacement_prevents_reservation_or_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    c.register().await.unwrap();
    fs::rename(dir.path().join("node"), dir.path().join("old")).unwrap();
    fs::create_dir(dir.path().join("node")).unwrap();
    fs::set_permissions(dir.path().join("node"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(c.reserve(request(c.identity(), 10)).is_err());
    assert!(c.state().attempts.is_empty());
}
#[tokio::test]
async fn registration_advertises_actual_backend_and_template_versions() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, config());
    let r = c.registration().unwrap();
    r.validate().unwrap();
    assert_eq!(r.schema_version, 2);
    assert_eq!(r.registration.identity, *c.identity());
    assert_eq!(
        r.backend.kind,
        aq_admission_sandbox::sandbox::BackendKind::DockerLocal
    );
    assert_eq!(r.backend.max_concurrent_sandboxes, 1);
    assert_eq!(r.backend.templates[0].harnesses[0].version, "1");
    assert_eq!(
        r.backend.capabilities,
        vec![aq_admission_sandbox::sandbox::BackendCapability::PersistentDiskDuringAttempt]
    );
}

#[tokio::test]
async fn run_registers_holds_root_and_drains_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let (tx, rx) = tokio::sync::watch::channel(false);
    {
        let service = c.run(rx);
        tokio::pin!(service);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut service)
                .await
                .is_err()
        );
        tx.send(true).unwrap();
        service.await.unwrap();
    }
    assert_eq!(c.inventory().refusal, Some(RefusalReason::Draining));
    assert!(c.state().attempts.is_empty());
}

#[test]
fn inventory_serialization_shows_measured_disk_fields_and_accepts_legacy_reports() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir, config());
    let mut inventory = c.inventory();
    inventory.disk_total_bytes = Some(4_096_000);
    inventory.disk_used_bytes = Some(2_867_200);
    inventory.disk_free_bytes = Some(1_024_000);
    inventory.preserved_bytes = Some(777);
    inventory.retained_snapshot_bytes = Some(1157);
    inventory.saved_snapshots_over_allowance = Some(1);
    let value = serde_json::to_value(&inventory).unwrap();
    for (field, expected) in [
        ("disk_total_bytes", 4_096_000u64),
        ("disk_used_bytes", 2_867_200),
        ("disk_free_bytes", 1_024_000),
        ("disk_reserved_bytes", 0),
        ("preserved_count", 0),
        ("preserved_bytes", 777),
        ("retained_snapshot_bytes", 1157),
        ("saved_snapshots_over_allowance", 1),
    ] {
        assert_eq!(value[field], expected, "{field}");
    }
    assert_eq!(
        serde_json::from_value::<Inventory>(value).unwrap(),
        inventory
    );

    // A report without measurements omits the fields; a legacy report decodes.
    let mut legacy = serde_json::to_value(c.inventory()).unwrap();
    let object = legacy.as_object_mut().unwrap();
    object.remove("disk_reserved_bytes");
    object.remove("preserved_count");
    assert!(!object.contains_key("disk_free_bytes") && !object.contains_key("preserved_bytes"));
    let decoded: Inventory = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.disk_free_bytes, None);
    assert_eq!(decoded.preserved_count, None);
    assert_eq!(decoded.available_disk_bytes, config().capacity.disk_bytes);
}

#[tokio::test]
async fn unavailable_provider_preserves_terminal_heartbeat_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = unavailable(&dir.path().join("node")).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let error = c
        .run_with_heartbeat(rx, async {
            tokio::task::yield_now().await;
            Err(aq_worker::client::CallError::Revoked)
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<aq_worker::client::CallError>(),
        Some(aq_worker::client::CallError::Revoked)
    ));
}

#[tokio::test]
async fn heartbeat_does_not_hide_controller_journal_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    fs::rename(dir.path().join("node"), dir.path().join("old")).unwrap();
    fs::create_dir(dir.path().join("node")).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let error = c
        .run_with_heartbeat(rx, std::future::pending())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "controller root changed");
    // A replaced root cannot persist draining; admission remains fenced.
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
}
