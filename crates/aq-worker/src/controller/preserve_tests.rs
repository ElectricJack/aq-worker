//! SBX22/SBX23 preserve bounds on the fake backend; explicit clock, no sleeps.
use super::recovery_tests::{Provider, running};
use super::tests::{config, request};
use super::*;
use std::fs;

const H: u64 = 60 * 60 * 1000;

fn open(dir: &tempfile::TempDir, cfg: NodeConfig) -> Controller<FakeBackend, Provider> {
    Controller::open(
        &dir.path().join("node"),
        Uuid::from_u128(1),
        cfg,
        FakeBackend::default(),
        Provider::default(),
    )
    .unwrap()
}
/// Runs attempt `n`, then stops it at its deadline with handoff unavailable.
async fn preserve(c: &mut Controller<FakeBackend, Provider>, n: u128) -> (Uuid, u64) {
    let id = running(c, n).await;
    let t = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(t).await.unwrap();
    c.retry_handoffs_at(t).await.unwrap();
    assert!(c.state().attempts[&id].preserved.is_some());
    (id, t)
}
fn notices(c: &Controller<FakeBackend, Provider>) -> Vec<PreserveNotice> {
    c.provider
        .emitted
        .iter()
        .map(|NodeEvent::SandboxPreserved { notice, .. }| *notice)
        .collect()
}
async fn disk_present(c: &mut Controller<FakeBackend, Provider>, id: Uuid) -> bool {
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    c.backend.inspect(&s).await.unwrap().disk_present
}

#[tokio::test]
async fn sbx22a_preserve_emits_one_sandbox_preserved_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let (id, t) = preserve(&mut c, 10).await;
    let want = NodeEvent::SandboxPreserved {
        attempt_id: id,
        node: c.identity().node_id,
        bytes_held: c.state().attempts[&id].request.disk_bytes,
        expires_at: t + PreserveConfig::DEFAULT_MAX_AGE_MS,
        notice: PreserveNotice::Preserved,
    };
    assert_eq!(c.provider.emitted, vec![want.clone()]);
    let wire = serde_json::to_value(&want).unwrap();
    assert_eq!(wire["kind"], "sandbox.preserved");
    assert_eq!(wire["expires_at"], t + 3 * DAY_MS);
    // Further failed handoffs before expiry do not re-notify.
    for k in 1..5 {
        c.retry_handoffs_at(t + k * H).await.unwrap();
    }
    assert_eq!(c.provider.emitted.len(), 1);
    // The disk reservation is still held while preserved.
    assert!(c.inventory().available_disk_bytes < config().capacity.disk_bytes);
}

#[tokio::test]
async fn sbx22b_handoff_backoff_is_bounded_and_success_destroys_in_same_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let (id, t) = preserve(&mut c, 10).await;
    let interval = c.config.recovery.sweep_interval_ms;
    let next = |c: &Controller<FakeBackend, Provider>| {
        c.state().attempts[&id]
            .preserved
            .as_ref()
            .unwrap()
            .next_handoff_ms
    };
    assert_eq!(next(&c), t + 2 * interval);
    // Not due: no handoff attempt.
    c.retry_handoffs_at(t + 1).await.unwrap();
    assert_eq!(c.provider.handoffs.len(), 1);
    let mut now = next(&c);
    c.retry_handoffs_at(now).await.unwrap();
    assert_eq!(c.provider.handoffs.len(), 2);
    assert_eq!(next(&c), now + 4 * interval);
    // Handoff succeeds before the 30s cleanup grace: destroyed in this pass.
    now = next(&c);
    assert!(
        now < c.state().attempts[&id]
            .cleanup
            .as_ref()
            .unwrap()
            .destroy_after_ms
    );
    c.provider.retention_available = true;
    c.retry_handoffs_at(now).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Destroyed);
    assert!(!disk_present(&mut c, id).await);
    assert_eq!(c.preserved_count(), 0);

    // The backoff never exceeds the bound, however many failures accrue.
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let (again, _) = preserve(&mut c, 10).await;
    assert_eq!(again, id, "same attempt id; `next` reads it");
    let mut now = next(&c);
    let mut gap = 0;
    for _ in 0..20 {
        c.retry_handoffs_at(now).await.unwrap();
        gap = next(&c) - now;
        assert!(gap <= MAX_HANDOFF_BACKOFF_MS, "gap {gap}");
        now = next(&c);
    }
    assert_eq!(gap, MAX_HANDOFF_BACKOFF_MS);
    assert_eq!(c.provider.handoffs.len(), 21);
}

#[tokio::test]
async fn sbx22c_max_count_refuses_new_offers_with_typed_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.max_concurrent_sandboxes = 4;
    cfg.preserve = PreserveConfig::vps();
    let mut c = open(&dir, cfg);
    let (id, _) = preserve(&mut c, 10).await;
    assert_eq!(c.preserved_count(), 1);
    let refused = c.reserve(request(c.identity(), 11)).unwrap_err();
    assert_eq!(
        *refused.downcast_ref::<RefusalReason>().unwrap(),
        RefusalReason::NodePreservedCapacityReached
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("node/journal.json")).unwrap()).unwrap();
    assert_eq!(
        saved["state"]["last_refusal"],
        "node_preserved_capacity_reached"
    );
    // Once handed off and destroyed, capacity frees up again.
    c.provider.retention_available = true;
    let due = c.state().attempts[&id]
        .preserved
        .as_ref()
        .unwrap()
        .next_handoff_ms;
    c.retry_handoffs_at(due).await.unwrap();
    assert_eq!(c.preserved_count(), 0);
    c.reserve(request(c.identity(), 11)).unwrap();
    // Presets per node class.
    assert_eq!(PreserveConfig::vps().max_count, 1);
    assert_eq!(PreserveConfig::desktop().max_count, 4);
    assert_eq!(PreserveConfig::boat().max_count, 10);
}

#[tokio::test]
async fn sbx22d_notify_expiry_reminds_at_most_daily_and_keeps_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let (id, t) = preserve(&mut c, 10).await;
    let age = PreserveConfig::DEFAULT_MAX_AGE_MS;
    for now in [
        t + age - 1,
        t + age,
        t + age + H,
        t + age + 23 * H,
        t + age + DAY_MS,
    ] {
        c.retry_handoffs_at(now).await.unwrap();
    }
    assert_eq!(
        notices(&c),
        vec![
            PreserveNotice::Preserved,
            PreserveNotice::Reminder,
            PreserveNotice::Reminder
        ]
    );
    let a = &c.state().attempts[&id];
    assert_eq!(a.phase, Phase::Stopped);
    assert!(a.destroy_authorization.is_none());
    assert!(disk_present(&mut c, id).await);
}

#[tokio::test]
async fn sbx22d_destroy_expiry_final_notice_then_standing_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.preserve.expiry_action = PreserveExpiryAction::Destroy;
    let mut c = open(&dir, cfg);
    let (id, t) = preserve(&mut c, 10).await;
    let age = PreserveConfig::DEFAULT_MAX_AGE_MS;
    c.retry_handoffs_at(t + age - DAY_MS - 1).await.unwrap();
    assert_eq!(notices(&c), vec![PreserveNotice::Preserved]);
    // Final notice undeliverable: destruction must wait for it.
    c.provider.events_offline = true;
    c.retry_handoffs_at(t + age - DAY_MS).await.unwrap();
    c.provider.events_offline = false;
    c.retry_handoffs_at(t + age - DAY_MS + H).await.unwrap();
    let notice_at = t + age - DAY_MS + H;
    assert_eq!(
        notices(&c),
        vec![PreserveNotice::Preserved, PreserveNotice::FinalNotice]
    );
    c.retry_handoffs_at(t + age).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
    c.retry_handoffs_at(notice_at + DAY_MS - 1).await.unwrap();
    assert!(disk_present(&mut c, id).await);
    c.retry_handoffs_at(notice_at + DAY_MS).await.unwrap();
    let a = c.state().attempts[&id].clone();
    assert_eq!(a.phase, Phase::Destroyed);
    let release = a.preserved.unwrap().standing_release.unwrap();
    assert!(release.starts_with("standing-release:preserve.expiry_action=destroy"));
    let sandbox = a.sandbox.unwrap();
    assert_eq!(
        a.destroy_authorization,
        Some(DestroyAuthorization::operator_released(sandbox, release.clone()).unwrap())
    );
    let journal = fs::read_to_string(dir.path().join("node/journal.json")).unwrap();
    assert!(journal.contains(&release));
    assert!(!disk_present(&mut c, id).await);
    assert_eq!(notices(&c).len(), 2);
}

#[tokio::test]
async fn sbx23_reachable_runner_drops_only_declared_paths_before_stop() {
    const KEEP: [&str; 2] = ["/workspace/src/main.rs", "/workspace/.git"];
    // Template defaults: CARGO_TARGET_DIR, ~/.cache, node_modules.
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let id = running(&mut c, 10).await;
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    let declared = PreserveConfig::DEFAULT_RECONSTRUCTIBLE_PATHS;
    c.backend
        .seed_paths(&s, KEEP.into_iter().chain(declared))
        .unwrap();
    let t = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(t).await.unwrap();
    c.retry_handoffs_at(t).await.unwrap();
    assert!(c.state().attempts[&id].preserved.is_some());
    assert_eq!(
        c.backend
            .paths(&s)
            .unwrap()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["/workspace/.git", "/workspace/src/main.rs"]
    );

    // A template declaring only node_modules keeps the other default names.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    let digest = format!("sha256:{}", cfg.templates[0].digest.as_str());
    cfg.preserve
        .reconstructible_paths
        .insert(digest, vec!["node_modules".into()]);
    let mut c = open(&dir, cfg);
    let id = running(&mut c, 10).await;
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    c.backend
        .seed_paths(&s, KEEP.into_iter().chain(declared))
        .unwrap();
    let t = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(t).await.unwrap();
    assert!(!c.backend.paths(&s).unwrap().contains("node_modules"));
    assert_eq!(c.backend.paths(&s).unwrap().len(), 4);
}

#[tokio::test]
async fn sbx23_unreachable_runner_stops_without_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = open(&dir, config());
    let id = running(&mut c, 10).await;
    let s = c.state().attempts[&id].sandbox.clone().unwrap();
    c.backend
        .seed_paths(&s, PreserveConfig::DEFAULT_RECONSTRUCTIBLE_PATHS)
        .unwrap();
    c.backend.inject(Fault::RunnerUnreachable);
    let t = c.state().attempts[&id].create.hard_deadline_ms;
    c.sweep_at(t).await.unwrap();
    c.retry_handoffs_at(t).await.unwrap();
    assert_eq!(c.state().attempts[&id].phase, Phase::Stopped);
    assert!(c.state().attempts[&id].preserved.is_some());
    assert_eq!(c.backend.paths(&s).unwrap().len(), 3);
}

#[test]
fn sbx22_boat_max_age_is_at_most_seven_days() {
    let mut p = PreserveConfig::boat();
    assert!(p.validate(BackendKind::Boat).is_ok());
    p.max_age_ms = BOAT_MAX_PRESERVE_AGE_MS + 1;
    assert!(p.validate(BackendKind::Boat).is_err());
    assert!(p.validate(BackendKind::DockerLocal).is_ok());
    let cfg: NodeConfig = serde_json::from_value({
        let mut v = serde_json::to_value(config()).unwrap();
        v.as_object_mut().unwrap().remove("preserve");
        v
    })
    .unwrap();
    assert_eq!(cfg.preserve, PreserveConfig::default());
    assert_eq!(cfg.preserve.expiry_action, PreserveExpiryAction::Notify);
}
