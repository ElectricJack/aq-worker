//! Actual filesystem/HTTP producer qualification, synthetic runtime policy.
use super::*;
use crate::native::preparation::{
    DurableInitialGrant, GrantCommit, QualifiedPresenceProfile, ScopedPresenceProducer,
};
use aq_admission::scoped_presence as wire;
fn profile(formatted: bool) -> (QualifiedPresenceProfile, ArtifactRef) {
    let runtime = qualification();
    let adapter = ArtifactRef {
        id: Text::new("synthetic.presence-adapter").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"synthetic producer adapter"),
    };
    let evidence = ArtifactRef {
        id: Text::new("synthetic.presence-evidence").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"synthetic proof only"),
    };
    let body = wire::Qualification {
        schema_version: 1,
        protocol: wire::Protocol::ScopedMaterialPresenceV1,
        runtime: runtime.runtime,
        runtime_qualification: runtime.qualification,
        adapter: adapter.clone(),
        adapter_qualification: evidence.clone(),
        inventory_semantics: wire::InventorySemantics::ProvisionableOnlyAndScopedPresenceV1,
        observation_protocol: wire::ObservationProtocol::GrantIssuanceBoundedBoottimeV1,
        maximum_age_ms: 30000,
        maximum_ttl_ms: 30000,
    };
    let bytes = if formatted {
        serde_json::to_vec_pretty(&body).unwrap()
    } else {
        canonical_bytes(&body).unwrap()
    };
    let reference = ArtifactRef {
        id: Text::new(format!("{}synthetic", wire::QUALIFICATION_PREFIX)).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&bytes),
    };
    let profile =
        QualifiedPresenceProfile::from_definition(reference, &bytes, &adapter, &evidence).unwrap();
    (profile, adapter)
}
async fn setup_window(
    window: u64,
) -> (Inputs, GitPeer, ScopedPresenceProducer, DurableInitialGrant) {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = granted_scenarios::joined(&h, &git).await;
    h.grant_window_ms.store(window, Ordering::SeqCst);
    let observation = granted_scenarios::observed(&h, inputs.request()).await;
    let GrantCommit::Usable(grant) = h
        .pipeline()
        .record_initial_grant(inputs, observation)
        .await
        .unwrap()
    else {
        panic!("synthetic initial grant expired")
    };
    let (profile, adapter) = profile(false);
    let producer = h
        .pipeline()
        .scoped_presence(profile, vec![adapter], 1)
        .await
        .unwrap();
    (h, git, producer, grant)
}
async fn setup() -> (Inputs, GitPeer, ScopedPresenceProducer, DurableInitialGrant) {
    setup_window(1000).await
}

async fn inventory(
    producer: &ScopedPresenceProducer,
) -> crate::native::preparation::AcknowledgedInventory {
    let op = producer.begin_inventory(1000).unwrap();
    producer.publish_inventory(&op).await.unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_private_read_publishes_exact_initial_scope_without_global_installation_claim() {
    let (h, _git, producer, grant) = setup().await;
    let ack = inventory(&producer).await;
    assert_eq!(ack.receipt().inventory.sequence, 2);
    assert_ne!(
        ack.receipt().inventory.snapshot_digest,
        ack.receipt().inventory.request_digest
    );
    let operation = producer.begin_presence(&grant, &ack, 1000).unwrap();
    producer.observe(&operation).await.unwrap();
    assert!(operation.report().unwrap().is_none());
    let receipt = producer.publish_presence(&operation).await.unwrap();
    let report = operation.report().unwrap().unwrap();
    assert!(matches!(report.presence, wire::Presence::Present { .. }));
    assert_eq!(report.authority.kind, wire::AuthorityKind::InitialGrant);
    assert_eq!(
        report.inventory.snapshot_digest,
        ack.receipt().inventory.snapshot_digest
    );
    assert!(
        report.observation.permit_acquired_ns <= report.observation.read_started_ns
            && report.observation.read_completed_ns <= report.observation.sent_ns
            && report.observation.sent_ns < report.observation.permit_deadline_ns
    );
    receipt.validate_for(&report).unwrap();
    let calls = h.scoped_calls.lock().unwrap();
    assert!(
        calls.iter().find(|(p, _)| p == "inventory").unwrap().1["provisionable_only"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("synthetic-key")
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
async fn material(h: &Inputs) -> PathBuf {
    let state = h.pipeline().snapshot().await.unwrap();
    let lease = state.attempts[&h.id]
        .initial_grant
        .as_ref()
        .unwrap()
        .delivery
        .original
        .materials[0]
        .lease_id;
    h._dir
        .path()
        .join("materials")
        .join(lease.to_string())
        .join("material.bin")
}
async fn pause_reached(pause: &super::super::WritePause) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
async fn expire(deadline: u64) {
    crate::native::http::test_clock::expire(deadline).await;
    assert!(boot_ns().unwrap() >= deadline);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn formatted_definition_is_accepted_and_pipeline_publisher_cannot_be_reset() {
    let (h, _git, producer, _grant) = setup().await;
    let (formatted, adapter) = profile(true);
    assert!(
        h.pipeline()
            .scoped_presence(formatted, vec![adapter], 1)
            .await
            .is_err()
    );
    drop(producer);
    let (formatted, adapter) = profile(true);
    assert!(
        h.pipeline()
            .scoped_presence(formatted, vec![adapter], 1)
            .await
            .is_err()
    );
    // Construction itself accepts the exact raw formatted Definition pin.
    let _ = profile(true);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_lost_ack_keeps_original_operation_seed_and_conservative_deadline() {
    let (h, _git, producer, _grant) = setup().await;
    h.scoped_mode.store(1, Ordering::SeqCst);
    let operation = producer.begin_inventory(1000).unwrap();
    assert_eq!(operation.request().sequence, 2);
    assert!(producer.publish_inventory(&operation).await.is_err());
    let after_failure = boot_ns().unwrap();
    assert!(producer.begin_inventory(1000).is_err());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    h.scoped_mode.store(0, Ordering::SeqCst);
    let ack = producer.publish_inventory(&operation).await.unwrap();
    assert!(ack.deadline_ns() <= after_failure + 1_000_000_000);
    let calls = h.scoped_calls.lock().unwrap();
    let requests: Vec<_> = calls
        .iter()
        .filter(|(path, _)| path == "inventory")
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].1, requests[1].1);
    drop(calls);
    let next = producer.begin_inventory(1000).unwrap();
    assert_eq!(next.request().sequence, 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_presence_retry_never_recaptures_private_bytes_or_extends_observation() {
    let (h, _git, producer, grant) = setup().await;
    let ack = inventory(&producer).await;
    let operation = producer.begin_presence(&grant, &ack, 1000).unwrap();
    producer.observe(&operation).await.unwrap();
    h.scoped_mode.store(2, Ordering::SeqCst);
    assert!(producer.publish_presence(&operation).await.is_err());
    let before = operation.report().unwrap().unwrap();
    assert!(producer.begin_presence(&grant, &ack, 1000).is_err());
    fs::write(
        material(&h).await,
        b"changed after original private observation",
    )
    .unwrap();
    producer.observe(&operation).await.unwrap();
    h.scoped_mode.store(0, Ordering::SeqCst);
    producer.publish_presence(&operation).await.unwrap();
    let after = operation.report().unwrap().unwrap();
    assert_eq!(
        canonical_bytes(&before).unwrap(),
        canonical_bytes(&after).unwrap()
    );
    let next = producer.begin_presence(&grant, &ack, 1000).unwrap();
    assert_eq!(next.sequence(), operation.sequence() + 1);
    producer.observe(&next).await.unwrap();
    producer.publish_presence(&next).await.unwrap();
    assert!(matches!(
        next.report().unwrap().unwrap().presence,
        wire::Presence::Missing {
            reason: wire::MissingReason::Changed
        }
    ));
    let calls = h.scoped_calls.lock().unwrap();
    let reports: Vec<_> = calls.iter().filter(|(p, _)| p == "presence").collect();
    assert_eq!(reports[0].1, reports[1].1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsafe_or_absent_private_members_never_become_positive() {
    use std::os::unix::fs::symlink;
    for mode in 0..5 {
        let (h, _git, producer, grant) = setup().await;
        let ack = inventory(&producer).await;
        let path = material(&h).await;
        match mode {
            0 => fs::remove_file(&path).unwrap(),
            1 => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            2 => {
                let other = h._dir.path().join("external-secret");
                fs::write(&other, b"must never be observed").unwrap();
                fs::remove_file(&path).unwrap();
                symlink(other, &path).unwrap();
            }
            3 => fs::hard_link(&path, h._dir.path().join("alias")).unwrap(),
            4 => {
                fs::remove_file(&path).unwrap();
                let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), producer.observe(&op))
            .await
            .unwrap()
            .unwrap();
        producer.publish_presence(&op).await.unwrap();
        let report = op.report().unwrap().unwrap();
        assert!(!report.presence.is_present());
        if mode == 0 {
            assert!(matches!(
                report.presence,
                wire::Presence::Missing {
                    reason: wire::MissingReason::Missing
                }
            ));
        } else {
            assert!(matches!(
                report.presence,
                wire::Presence::Unknown {
                    reason: wire::UnknownReason::Unreadable
                }
            ));
        }
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_installation_record_is_negative_without_serializing_untrusted_record() {
    let (h, _git, producer, grant) = setup().await;
    let ack = inventory(&producer).await;
    let path = material(&h).await.with_file_name("record.json");
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["operation_id"] = json!(Uuid::new_v4());
    fs::write(path, canonical_bytes(&raw).unwrap()).unwrap();
    let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
    producer.observe(&op).await.unwrap();
    producer.publish_presence(&op).await.unwrap();
    assert!(matches!(
        op.report().unwrap().unwrap().presence,
        wire::Presence::Missing {
            reason: wire::MissingReason::Changed
        }
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn positive_cannot_survive_expiry_during_read_but_scoped_missing_can() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    for missing in [false, true] {
        let (h, _git, producer, grant) = setup_window(200).await;
        let producer = Arc::new(producer);
        let ack = inventory(&producer).await;
        let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
        let path = material(&h).await;
        let pause = super::super::WritePause::new();
        producer.test_pause_read(&pause).await.unwrap();
        let owner = producer.clone();
        let observed = op.clone();
        let job = tokio::spawn(async move { owner.observe(&observed).await });
        pause_reached(&pause).await;
        if missing {
            fs::remove_file(path).unwrap();
        }
        expire(grant.deadline_ns()).await;
        pause.release();
        let result = job.await.unwrap();
        if missing {
            result.unwrap();
            producer.publish_presence(&op).await.unwrap();
            let report = op.report().unwrap().unwrap();
            assert!(!report.presence.is_present());
            assert!(report.observation.sent_ns >= report.observation.permit_deadline_ns);
        } else {
            assert!(result.is_err());
            assert!(producer.publish_presence(&op).await.is_err());
        }
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_actual_read_remains_owned_and_is_published_only_by_explicit_retry() {
    let (h, _git, producer, grant) = setup().await;
    let producer = Arc::new(producer);
    let ack = inventory(&producer).await;
    let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
    let pause = super::super::WritePause::new();
    producer.test_pause_read(&pause).await.unwrap();
    let owner = producer.clone();
    let pending = op.clone();
    let task = tokio::spawn(async move { owner.observe(&pending).await });
    pause_reached(&pause).await;
    task.abort();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    assert!(producer.begin_presence(&grant, &ack, 1000).is_err());
    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match producer.observe(&op).await {
                Ok(()) => break,
                Err(e) if e.to_string().contains("running") => {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await
                }
                Err(_) => panic!("unexpected read owner failure"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        h.scoped_calls
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| p != "presence")
    );
    producer
        .publish_presence(&producer.current_presence(h.id).unwrap().unwrap())
        .await
        .unwrap();
    assert_eq!(op.sequence(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_or_oversized_receipts_fail_closed_and_keep_identical_operation() {
    for mode in [3, 4] {
        let (h, _git, producer, grant) = setup().await;
        let ack = inventory(&producer).await;
        let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
        producer.observe(&op).await.unwrap();
        h.scoped_mode.store(mode, Ordering::SeqCst);
        assert!(producer.publish_presence(&op).await.is_err());
        let before = op.report().unwrap().unwrap();
        assert!(producer.begin_presence(&grant, &ack, 1000).is_err());
        h.scoped_mode.store(0, Ordering::SeqCst);
        producer.publish_presence(&op).await.unwrap();
        assert_eq!(
            canonical_bytes(&before).unwrap(),
            canonical_bytes(&op.report().unwrap().unwrap()).unwrap()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_factory_does_not_burn_singleton_and_old_inventory_cannot_name_new_report() {
    let h = Inputs::new(synthetic()).await;
    let (q, _) = profile(false);
    assert!(h.pipeline().scoped_presence(q, vec![], 1).await.is_err());
    let (q, a) = profile(true);
    let owner = h.pipeline().scoped_presence(q, vec![a], 1).await.unwrap();
    let first = owner.begin_inventory(1000).unwrap();
    let ack = owner.publish_inventory(&first).await.unwrap();
    assert_eq!(ack.receipt().inventory.sequence, 2);
    let (_h, _git, producer, grant) = setup().await;
    let original = inventory(&producer).await;
    let next = producer.begin_inventory(1000).unwrap();
    let newer = producer.publish_inventory(&next).await.unwrap();
    assert!(producer.begin_presence(&grant, &original, 1000).is_err());
    assert!(producer.begin_presence(&grant, &newer, 1000).is_ok());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_network_publication_keeps_original_body_for_exact_retry() {
    let (h, _git, producer, grant) = setup().await;
    let producer = Arc::new(producer);
    let ack = inventory(&producer).await;
    let operation = producer.begin_presence(&grant, &ack, 1000).unwrap();
    producer.observe(&operation).await.unwrap();
    h.scoped_mode.store(5, Ordering::SeqCst);
    let owner = producer.clone();
    let op = operation.clone();
    let task = tokio::spawn(async move { owner.publish_presence(&op).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.scoped_entered.acquire(),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    let body = canonical_bytes(&operation.report().unwrap().unwrap()).unwrap();
    task.abort();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    h.scoped_mode.store(0, Ordering::SeqCst);
    producer.publish_presence(&operation).await.unwrap();
    assert_eq!(
        body,
        canonical_bytes(&operation.report().unwrap().unwrap()).unwrap()
    );
    h.scoped_release.add_permits(1);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_initial_token_never_reads_or_publishes_positive() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let (h, _git, producer, grant) = setup_window(100).await;
    // Exceed the original 100ms wall interval while fixture authority is frozen.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(boot_ns().unwrap() < grant.deadline_ns());
    let ack = inventory(&producer).await;
    let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
    clock.advance_to(grant.deadline_ns());
    assert!(producer.observe(&op).await.is_err());
    assert!(producer.publish_presence(&op).await.is_err());
    assert!(
        h.scoped_calls
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| p != "presence")
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_replacement_during_actual_read_is_never_positive_even_with_same_bytes() {
    for member in ["material.bin", "record.json", "directory"] {
        let (h, _git, producer, grant) = setup().await;
        let producer = Arc::new(producer);
        let ack = inventory(&producer).await;
        let path = material(&h).await;
        let dir = path.parent().unwrap().to_path_buf();
        let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
        let pause = super::super::WritePause::new();
        producer.test_pause_read(&pause).await.unwrap();
        let owner = producer.clone();
        let pending = op.clone();
        let task = tokio::spawn(async move { owner.observe(&pending).await });
        pause_reached(&pause).await;
        if member == "directory" {
            let record = fs::read(dir.join("record.json")).unwrap();
            let material = fs::read(&path).unwrap();
            fs::rename(&dir, dir.with_extension("original-held")).unwrap();
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            for (name, bytes) in [("record.json", record), ("material.bin", material)] {
                let target = dir.join(name);
                fs::write(&target, bytes).unwrap();
                fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
            }
        } else {
            let target = dir.join(member);
            let replacement = dir.join("replacement");
            fs::write(&replacement, fs::read(&target).unwrap()).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
            fs::rename(replacement, target).unwrap();
        }
        pause.release();
        task.await.unwrap().unwrap();
        producer.publish_presence(&op).await.unwrap();
        assert!(matches!(
            op.report().unwrap().unwrap().presence,
            wire::Presence::Missing {
                reason: wire::MissingReason::Changed
            }
        ));
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_read_interval_is_bounded_even_with_a_still_live_initial_grant() {
    let (h, _git, producer, grant) = setup_window(5000).await;
    let producer = Arc::new(producer);
    let ack = inventory(&producer).await;
    let op = producer.begin_presence(&grant, &ack, 1000).unwrap();
    let pause = super::super::WritePause::new();
    producer.test_pause_read(&pause).await.unwrap();
    let owner = producer.clone();
    let pending = op.clone();
    let task = tokio::spawn(async move { owner.observe(&pending).await });
    pause_reached(&pause).await;
    let after_entered = boot_ns().unwrap();
    expire(after_entered + wire::MAX_READ_MS * 1_000_000).await;
    assert!(boot_ns().unwrap() < grant.deadline_ns());
    pause.release();
    let result = task.await.unwrap();
    assert!(matches!(result,Err(e) if e.to_string().contains("read interval exceeded")));
    assert!(producer.publish_presence(&op).await.is_err());
    assert!(
        h.scoped_calls
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| p != "presence")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_private_files_and_received_grant_cannot_construct_a_new_positive_producer() {
    let (mut h, _git, producer, grant) = setup().await;
    let state = h.pipeline().snapshot().await.unwrap();
    let receipt = state.attempts[&h.id]
        .prepared
        .as_ref()
        .unwrap()
        .request
        .materials[0]
        .clone();
    drop(producer);
    drop(grant);
    drop(h.pipeline.take());
    let mut limits = limits();
    limits.disk_bytes = 64 * 1024 * 1024 * 1024;
    let journal = Journal::open(&h.root, identity().node_id, limits).unwrap();
    assert!(journal.available().is_err());
    assert!(journal.state().attempts[&h.id].initial_grant.is_some());
    let materials = MaterialStore::open(&h._dir.path().join("materials"), identity(), 32).unwrap();
    assert_eq!(
        materials
            .reopen_pinned(&receipt)
            .unwrap()
            .read_private()
            .unwrap()
            .as_slice(),
        b"synthetic-key"
    );
    let pipeline = Arc::new(InputPipeline::new(journal, materials, h.client.clone()));
    let (q, a) = profile(false);
    assert!(pipeline.scoped_presence(q, vec![a], 2).await.is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opaque_token_and_inventory_from_other_pipeline_cannot_be_rebound() {
    let (h, _git, producer, grant) = setup().await;
    let (_h2, _git2, producer2, grant2) = setup().await;
    let ack = inventory(&producer).await;
    let ack2 = inventory(&producer2).await;
    assert!(producer.begin_presence(&grant2, &ack, 1000).is_err());
    assert!(producer.begin_presence(&grant, &ack2, 1000).is_err());
    assert!(
        h.scoped_calls
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| p != "presence")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_bulk_reads_do_not_consume_scoped_control_publication() {
    let (h, _git, producer, _grant) = setup().await;
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation().clone();
    let id = h.id;
    let bulk = tokio::spawn(async move { pipeline.artifacts(id, preparation).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.inline_entered.acquire_many(2),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    let op = producer.begin_inventory(1000).unwrap();
    let ack = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        producer.publish_inventory(&op),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ack.receipt().inventory.sequence, 2);
    h.inline_release.add_permits(1024);
    bulk.await.unwrap().unwrap();
}
