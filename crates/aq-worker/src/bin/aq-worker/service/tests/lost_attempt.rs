use super::guardian::running;
use super::*;

/// D2 under the proof-format ruling, in the service's terms: a lost guardian is held,
/// never reported stopped. The reason names the outside proof found from real facts: a
/// changed boot id, or the unit teardown attested from a cgroup holding only this process
/// (a fake cgroup file); with a foreign pid there is no teardown proof.
#[test]
fn a_lost_guardian_is_held_and_names_its_outside_proof() {
    use aq_node_service::invocation::{holds_only, own_cgroup_procs, unit_of};
    use aq_session_host::lost_guardian::{PriorGone, recover_stopped_with};
    const OLD: &str = "00000000-0000-4000-8000-00000000000a";
    const NOW: &str = "00000000-0000-4000-8000-00000000000b";
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("runtime-wiped");
    let held = |last: &aq_session_host::HostObservation, prior: Option<&PriorGone>| {
        let result = recover_stopped_with(&gone, &last.guardian.intent, Some(last), NOW, prior);
        let (proof, why) = unproven(&result).unwrap();
        assert_eq!(
            proof.is_some(),
            !why.contains("host stop unproven"),
            "{why}"
        );
        why
    };
    let unit_dir = dir.path().join("app.slice/aq-node@t.service");
    std::fs::create_dir_all(&unit_dir).unwrap();
    let procs = own_cgroup_procs("0::/app.slice/aq-node@t.service\n", dir.path()).unwrap();
    let own = std::process::id();
    let attested = |listed: &str| {
        std::fs::write(&procs, listed).unwrap();
        let read = std::fs::read_to_string(&procs).unwrap();
        let gone = holds_only(&read, own, |pid| (pid == own + 1).then_some(own));
        gone.then(|| PriorGone {
            boot: NOW.into(),
            start_ticks: 900,
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: unit_of(&procs).unwrap(),
        })
    };
    let rebooted = held(&running(OLD), None);
    assert!(
        rebooted.contains("\"proof_kind\":\"boot_changed\""),
        "{rebooted}"
    );
    let ours = attested(&format!("{own}\n{}\n", own + 1));
    let torn = held(&running(NOW), ours.as_ref());
    assert!(
        torn.contains("\"proof_kind\":\"cgroup_teardown\""),
        "{torn}"
    );
    assert!(torn.contains("aq-node@t.service"), "{torn}");
    let foreign = attested(&format!("{own}\n1\n"));
    assert!(foreign.is_none(), "a foreign pid: no attestation");
    let none = held(&running(NOW), foreign.as_ref());
    assert!(none.contains("host stop unproven"), "{none}");
    assert!(unproven(&Ok::<_, anyhow::Error>(())).is_none());
}

fn test_loss(attempt: Uuid, proof: aq_protocol::StopProof) -> LostAttempt<'static> {
    LostAttempt {
        attempt,
        proof,
        claim_unreported: false,
        why: "lost",
    }
}

fn teardown(invocation: &str) -> aq_protocol::StopProof {
    aq_protocol::StopProof::CgroupTeardown {
        invocation_id: invocation.repeat(32 / invocation.len()),
        unit: "aq-node@t.service".into(),
    }
}
fn answer(r: &aq_protocol::LostAttemptRelease, released: bool) -> aq_protocol::LostAttemptReleased {
    aq_protocol::LostAttemptReleased {
        schema_version: 1,
        attempt_id: r.attempt_id,
        reason: aq_protocol::LostReason::of(&r.proof).unwrap(),
        charged: true,
        released,
        held: (!released).then(|| "lineage_retry_budget_exhausted".into()),
    }
}
fn record(dir: &Path, attempt: Uuid) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(preserved_sandbox_record(dir, attempt)).unwrap()).unwrap()
}

/// D2 ruling C: with an outside proof, the lost attempt's sandbox is stopped (kept), the
/// release is posted with the proof, the record says nothing was salvaged, and the slot
/// is freed. A refused release or a sandbox that cannot be stopped holds the slot.
#[tokio::test]
async fn a_proven_lost_attempt_is_released_and_frees_the_slot() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let dir = tempfile::tempdir().unwrap();
    let attempt = Uuid::from_u128(77);
    let proof = teardown("a");
    let stopped = AtomicBool::new(false);
    let posted = std::sync::Mutex::new(None);
    let outcome = release_lost(
        async || {
            stopped.store(true, SeqCst);
            Ok(())
        },
        async |r| {
            anyhow::ensure!(stopped.load(SeqCst), "stopped before the post");
            *posted.lock().unwrap() = Some(r.clone());
            Ok(answer(&r, true))
        },
        dir.path(),
        test_loss(attempt, proof.clone()),
    )
    .await;
    assert_eq!(outcome, RecoverOutcome::Lost);
    let sent = posted.lock().unwrap().clone().unwrap();
    assert_eq!((sent.attempt_id, &sent.proof), (attempt, &proof));
    let record = record(dir.path(), attempt);
    assert_eq!(record["salvaged"], false);
    assert_eq!(record["reason"], "lost_after_crash");
    assert!(
        record["sandbox"]
            .as_str()
            .unwrap()
            .contains("release-sandbox")
    );
    let refused = release_lost(
        async || Ok(()),
        async |_| anyhow::bail!("409 lost_attempt_effects_present"),
        dir.path(),
        test_loss(Uuid::from_u128(78), proof.clone()),
    )
    .await;
    assert!(matches!(refused, RecoverOutcome::Held(why) if why.contains("refused")));
    let unstoppable = release_lost(
        async || anyhow::bail!("docker down"),
        async |_| panic!("never posted while the sandbox may run"),
        dir.path(),
        test_loss(Uuid::from_u128(79), proof),
    )
    .await;
    assert!(
        matches!(unstoppable, RecoverOutcome::Held(why) if why.contains("could not be stopped"))
    );
}

/// #51 review I1: the first proof is stored before the post and reused by every retry and
/// restart (the attestation is new after each restart), and a refusal for "another proof"
/// means the attempt was already released: the slot is freed.
#[tokio::test]
async fn a_lost_release_retry_reuses_its_first_proof() {
    let dir = tempfile::tempdir().unwrap();
    let attempt = Uuid::from_u128(80);
    let sent = std::sync::Mutex::new(vec![]);
    let refused = release_lost(
        async || Ok(()),
        async |r| {
            sent.lock().unwrap().push(r.proof.clone());
            anyhow::bail!("server unreachable")
        },
        dir.path(),
        test_loss(attempt, teardown("a")),
    )
    .await;
    assert!(matches!(refused, RecoverOutcome::Held(_)));
    // After a restart the service attests a new invocation: the first proof is still sent.
    let again = release_lost(
        async || Ok(()),
        async |r| {
            sent.lock().unwrap().push(r.proof.clone());
            Ok(answer(&r, true))
        },
        dir.path(),
        test_loss(attempt, teardown("b")),
    )
    .await;
    assert_eq!(again, RecoverOutcome::Lost);
    assert_eq!(*sent.lock().unwrap(), [teardown("a"), teardown("a")]);
    let other = release_lost(
        async || Ok(()),
        async |_| anyhow::bail!("409 conflict: lost_attempt_released_with_another_proof"),
        dir.path(),
        test_loss(Uuid::from_u128(81), teardown("c")),
    )
    .await;
    assert_eq!(other, RecoverOutcome::Lost, "already released");
}

/// #117 review I1, worker side: a release the server answers with the task still held
/// (an exhausted budget) keeps the slot held, with the server's reason.
#[tokio::test]
async fn a_budget_held_lost_release_keeps_the_slot_held() {
    let dir = tempfile::tempdir().unwrap();
    let attempt = Uuid::from_u128(82);
    let outcome = release_lost(
        async || Ok(()),
        async |r| Ok(answer(&r, false)),
        dir.path(),
        test_loss(attempt, teardown("a")),
    )
    .await;
    assert!(
        matches!(&outcome, RecoverOutcome::Held(why) if why.contains("lineage_retry_budget_exhausted")),
        "{outcome:?}"
    );
    assert_eq!(record(dir.path(), attempt)["released"], false);
}

/// #51 review I2: a lost attempt whose result claim was recorded before the cut keeps its
/// sandbox (settle's release skips every lost-path attempt), and its record tells the
/// operator the work is in the archived worktree.
#[tokio::test]
async fn a_lost_attempt_with_a_claim_keeps_its_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let (lost, claimed) = (Uuid::from_u128(83), Uuid::from_u128(84));
    let outcome = release_lost(
        async || Ok(()),
        async |r| Ok(answer(&r, true)),
        dir.path(),
        LostAttempt {
            claim_unreported: true,
            ..test_loss(lost, teardown("a"))
        },
    )
    .await;
    assert_eq!(outcome, RecoverOutcome::Lost);
    let record = record(dir.path(), lost);
    assert_eq!(record["claim_unreported"], true);
    assert!(
        record["note"]
            .as_str()
            .unwrap()
            .contains("archived worktree")
    );
    assert_eq!(destroyable(dir.path(), vec![lost, claimed]), [claimed]);
}

/// R4: slot recovery overflowed tokio's 2 MiB worker stack (a debug build needed about
/// 2.2 MiB). Every future on the recovery path is boxed below 16 KiB, so a regression
/// that inlines a large one again fails here.
#[test]
fn recovery_futures_stay_small() {
    fn size<A, F>(_: impl FnOnce(A) -> F) -> usize {
        std::mem::size_of::<F>()
    }
    const LIMIT: usize = 16 << 10;
    let worker: &'static WorkerConfig = Box::leak(Box::new(WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::nil(),
        credential: "c".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }));
    let path = PathBuf::from("/nonexistent");
    let backend: &'static NativeSlotBackend = Box::leak(Box::new(NativeSlotBackend::new(path)));
    // `recover_attempts` is the body the async_trait `recover` boxes. It awaits
    // `release_lost` with the real closures (a docker backend and the server link), so the
    // lost-guardian release is measured inside it as instantiated in the service.
    let sizes = [
        (
            "recover_attempts",
            size(|c: SlotContext| backend.recover_attempts(c)),
        ),
        (
            "salvage_interrupted",
            size(|c: &'static NativeConfig| salvage_interrupted(worker, c, Uuid::nil())),
        ),
        (
            "reconcile_native",
            size(|c: &'static NativeConfig| {
                aq_worker::native::runner::reconcile_native(worker, c, Uuid::nil())
            }),
        ),
    ];
    for (name, bytes) in sizes {
        assert!(
            bytes < LIMIT,
            "{name} future is {bytes} bytes (limit {LIMIT})"
        );
    }
}
/// R4 (b): the lost-guardian release, within half a 2 MiB tokio worker stack: the sandbox
/// stop, the stored proof, the release post and the lost record, then a freed slot.
#[test]
fn a_lost_guardian_release_fits_half_a_worker_stack() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_owned();
    let run = move || {
        let mut rt = tokio::runtime::Builder::new_multi_thread();
        let rt = rt.worker_threads(1).thread_stack_size(1 << 20);
        let rt = rt.enable_all().build().unwrap();
        rt.block_on(rt.spawn(async move {
            let attempt = Uuid::from_u128(80);
            let post = async |r: aq_protocol::LostAttemptRelease| Ok(answer(&r, true));
            let proof = teardown("b");
            let released = release_lost(async || Ok(()), post, &state, test_loss(attempt, proof));
            (Box::pin(released).await, record(&state, attempt))
        }))
        .unwrap()
    };
    let (outcome, record) = std::thread::spawn(run).join().unwrap();
    assert_eq!(outcome, RecoverOutcome::Lost);
    assert_eq!(record["released"], true);
}

/// The lost attempt's running sandbox is stopped, not destroyed; others are untouched.
#[tokio::test]
async fn a_lost_attempts_sandbox_is_stopped_and_kept() {
    use aq_worker::backend::{
        CreateRequest, FakeBackend, Ownership, PlacementBackend, PrivateMaterial, SandboxState,
        TemplateDigest,
    };
    let mut backend = FakeBackend::default();
    let node = Uuid::from_u128(1);
    let template = TemplateDigest::parse(format!("sha256:{}", "1".repeat(64))).unwrap();
    let (lost, other) = (Uuid::new_v4(), Uuid::new_v4());
    let shared_boot = Uuid::new_v4();
    for attempt_id in [lost, other] {
        let owner = Ownership {
            node_id: node,
            boot_id: shared_boot,
            attempt_id,
        };
        let request = CreateRequest::new(owner, template.clone(), 1 << 40, vec![]);
        let sandbox = backend.create(&request).await.unwrap();
        let material = PrivateMaterial::new(b"fixture".to_vec());
        backend
            .deliver_private_material(&sandbox, material)
            .await
            .unwrap();
        backend.start_runner(&sandbox).await.unwrap();
    }
    stop_attempt_sandboxes(&mut backend, node, lost)
        .await
        .unwrap();
    let states: std::collections::BTreeMap<Uuid, SandboxState> =
        (backend.list_owned(node).await.unwrap())
            .into_iter()
            .map(|e| (e.sandbox.owner.attempt_id, e.state))
            .collect();
    assert_eq!(
        states[&lost],
        SandboxState::Stopped,
        "kept for the operator"
    );
    assert_eq!(states[&other], SandboxState::Running);
}

/// #51 re-review: when the lost-attempt record (the kept-sandbox marker) cannot be written,
/// the slot is held, never freed, even though the server released the attempt.
#[tokio::test]
async fn an_unwritten_lost_record_holds_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let attempt = Uuid::from_u128(86);
    // A directory where the record goes: the write fails.
    std::fs::create_dir(preserved_sandbox_record(dir.path(), attempt)).unwrap();
    let outcome = release_lost(
        async || Ok(()),
        async |r| Ok(answer(&r, true)),
        dir.path(),
        LostAttempt {
            claim_unreported: true,
            ..test_loss(attempt, teardown("a"))
        },
    )
    .await;
    assert!(
        matches!(&outcome, RecoverOutcome::Held(why) if why.contains("record was not written")),
        "{outcome:?}"
    );
}
