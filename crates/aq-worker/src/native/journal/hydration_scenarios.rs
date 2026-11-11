fn published_phase_fixture() -> serde_json::Value {
    serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
    ))
    .unwrap()
}
fn refresh_registry_fact(
    f: &mut aq_admission::preparation_v2::OwnerFact,
    c: &aq_admission::preparation_v2::ChoicePins,
    _: &aq_admission::preparation_v2::AcceptedReceipt,
) {
    if let aq_admission::preparation_v2::FactValue::Registry {
        registry,
        credential_profile,
        credential_revision,
    } = &mut f.body.value
    {
        *registry = c.registry.clone();
        *credential_profile = c.credential_profile.clone();
        *credential_revision = c.credential_revision;
    }
}
super::physical_fixture::physical_fixture! {aq_admission::preparation_v2,aq_phase_terminal_protocol,preparation_v2,phase_acceptance,phase_preparation,propose_preparation,begin_phase_accept,accept_preparation,record_phase_preparation}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_native_pipeline_journals_every_external_intent_and_keeps_lost_import_id() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    git.mode.store(1, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let first = h.pipeline().snapshot().await.unwrap();
    let imported = first.attempts[&h.id].source.as_ref().unwrap().operations["import"].clone();
    assert!(imported.result.is_none());
    assert!(
        !first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .operations
            .contains_key("create")
    );
    let spec = h.spec().await;
    let proof = h
        .pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
        .await
        .unwrap();
    assert_eq!(proof.attempt_id(), h.id);
    assert_eq!(proof.manifest().source.base.hex, "a".repeat(40));
    assert!(proof.workspace().path.exists());
    let calls = git.calls.lock().unwrap();
    let imports: Vec<_> = calls
        .iter()
        .filter(|r| r["operation"] == "artifact.import")
        .collect();
    assert_eq!(imports.len(), 2);
    assert_eq!(imports[0], imports[1]);
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["operation"] == "workspace.create")
            .count(),
        1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_receipts_cannot_bypass_fresh_retention_or_head_checks() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
        .await
        .unwrap();
    git.mode.store(2, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    git.mode.store(3, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(
        state.attempts[&h.id].source.as_ref().unwrap().operations["import"]
            .result
            .is_some()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_range_is_exact_pending_and_format_two_is_never_upgraded() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.mode.store(1, Ordering::SeqCst);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let first = h.pipeline().snapshot().await.unwrap();
    let range = first.attempts[&h.id]
        .source
        .as_ref()
        .unwrap()
        .pending
        .clone()
        .unwrap();
    assert_eq!(range.offset, 0);
    assert_eq!(
        first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .manifest_offset,
        0
    );
    h.mode.store(0, Ordering::SeqCst);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 2);
    let bytes = fs::read(root.join("journal.json")).unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), bytes);
}
fn git_request(
    exe: &Path,
    root: &Path,
    repo: Uuid,
    operation: &str,
    params: Value,
    generation: u64,
) -> Value {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .arg("--state-dir")
        .arg(root)
        .args(["request", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let id = Uuid::new_v4();
    let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"operation_id":id,"repository_id":repo,"expected_binding_generation":generation,"params":params});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        output.status.success() && value["ok"] == true,
        "public Git fixture failed: {value}"
    );
    value["result"].clone()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_native_source_http_import_retention_and_clean_workspace() {
    actual_git_hydration(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_received_initial_grant_retains_actual_workspace_and_material_owners() {
    actual_git_hydration(true, false).await;
}
async fn actual_git_hydration(retain_grant: bool, phase_mode: bool) {
    use std::process::Command;
    let exe =
        PathBuf::from(std::env::var_os("AQ_GIT_TEST_BINARY").expect("explicit public Git binary"));
    let pin = std::env::var("AQ_GIT_TEST_SHA256").expect("explicit binary SHA256");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&exe).unwrap())),
        pin
    );
    assert_eq!(pin.len(), 64);
    let dir = tempfile::tempdir().unwrap();
    let repository = dir.path().join("repository");
    fs::create_dir(&repository).unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "-q", "--initial-branch=main"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        repository.join("native-input.txt"),
        b"synthetic native source bytes\n",
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args(["add", "native-input.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args([
                "-c",
                "user.name=SyntheticFixture",
                "-c",
                "user.email=fixture@invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "native source base"
            ])
            .status()
            .unwrap()
            .success()
    );
    let hex = String::from_utf8(
        Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let oid = json!({"algorithm":"sha1","hex":hex});
    let rid = Uuid::new_v4();
    let central = dir.path().join("central");
    let worker = dir.path().join("worker");
    git_request(
        &exe,
        &central,
        rid,
        "repo.register",
        json!({"binding":{"identity":"synthetic","mode":"central","binding_generation":1,"remote_url":repository,"allow_file_transport":true,"allowed_refs":["refs/heads/main"]}}),
        0,
    );
    git_request(&exe, &central, rid, "repo.sync", json!({}), 1);
    let workspace = git_request(
        &exe,
        &central,
        rid,
        "workspace.create",
        json!({"base":oid}),
        1,
    );
    let checkpoint = git_request(
        &exe,
        &central,
        rid,
        "workspace.checkpoint",
        json!({"workspace_id":workspace["workspace_id"],"expected_generation":workspace["generation"],"expected_head":oid,"quiescent":true}),
        1,
    );
    let exported = git_request(
        &exe,
        &central,
        rid,
        "artifact.export",
        json!({"base":oid,"source":checkpoint["source"],"retention_handle":checkpoint["retention_handle"]}),
        1,
    );
    let artifact = Uuid::parse_str(exported["artifact_handle"].as_str().unwrap()).unwrap();
    let manifest_bytes = fs::read(
        central
            .join("artifacts")
            .join(artifact.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    let bundle = fs::read(
        central
            .join("artifacts")
            .join(artifact.to_string())
            .join("objects.bundle"),
    )
    .unwrap();
    let native: GitManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert!(!native.reference_only && !bundle.is_empty());
    let source = SourceBinding {
        repository_id: rid,
        binding_generation: 1,
        logical_identity: "synthetic".into(),
        base: serde_json::from_value(oid).unwrap(),
    };
    let manifest = TransferManifest {
        transfer_id: artifact,
        artifact_id: artifact,
        role: SourceRole::Base,
        attempt: AttemptRef {
            attempt_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(1),
            generation: 1,
        },
        source: source.clone(),
        checkpoint: source.base,
        resolved_spec_digest: "a".repeat(64),
        manifest_bytes: manifest_bytes.len() as u64,
        manifest_digest: format!("{:x}", Sha256::digest(&manifest_bytes)),
        bundle_bytes: bundle.len() as u64,
        bundle_digest: Some(format!("{:x}", Sha256::digest(&bundle))),
    };
    git_request(
        &exe,
        &worker,
        rid,
        "repo.register",
        json!({"binding":{"identity":"synthetic","mode":"worker","binding_generation":1}}),
        0,
    );
    let service = crate::source::GitService::start(&exe, &pin, &worker).unwrap();
    let h = Inputs::new_count(
        Export {
            manifest,
            manifest_bytes,
            bundle,
        },
        phase_mode,
        if phase_mode { 126 } else { 0 },
    )
    .await;
    if phase_mode {
        let joined = phase_join(&h, service.client.clone()).await;
        assert_eq!(
            fs::read(joined.workspace().workspace().path.join("native-input.txt")).unwrap(),
            b"synthetic native source bytes\n"
        );
        assert_eq!(
            joined.materials()[0].read_private().unwrap().as_slice(),
            b"synthetic-key"
        );
        assert!(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .phase_prepared
                .is_some()
        );
        h.grant_window_ms.store(5000, Ordering::SeqCst);
        let state = h.pipeline().snapshot().await.unwrap();
        let a = &state.attempts[&h.id];
        let intent = a.phase_acceptance.as_ref().unwrap();
        let observed = h
            .client
            .prepare_execution(
                h.proposed.as_ref().unwrap(),
                &intent.request,
                h.phase.as_ref().unwrap(),
                joined.request(),
                None,
            )
            .await
            .unwrap();
        let crate::native::preparation::PhaseExecutionCommit::Usable(owner) = h
            .pipeline()
            .record_phase_execution(joined, observed)
            .await
            .unwrap()
        else {
            panic!("actual Git phase execution observation not usable")
        };
        h.pipeline().check_phase_execution(&owner).await.unwrap();
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        return;
    }
    let spec = h.spec().await;
    let proof = h
        .pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, service.client.clone())
        .await
        .unwrap();
    assert_eq!(proof.workspace().generation, 1);
    assert_eq!(proof.manifest().source.repository_id, rid);
    assert!(
        proof
            .workspace()
            .path
            .starts_with(worker.join("workspaces"))
    );
    assert_eq!(
        fs::read(proof.workspace().path.join("native-input.txt")).unwrap(),
        b"synthetic native source bytes\n"
    );
    let all = h.all_inputs().await;
    let renderer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .startup
        .renderer_qualification
        .clone();
    let joined = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, proof, renderer)
        .await
        .unwrap();
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .prepared
            .is_some()
    );
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    if retain_grant {
        let offer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .reservation
            .offer
            .clone();
        let observed = h
            .client
            .prepare(joined.request(), h.preparation(), &offer, None)
            .await
            .unwrap();
        let committed = h
            .pipeline()
            .record_initial_grant(joined, observed)
            .await
            .unwrap();
        let crate::native::preparation::GrantCommit::Usable(owner) = committed else {
            panic!("actual Git initial grant expired or changed before commit")
        };
        h.pipeline().check_initial_grant(&owner).await.unwrap();
        let state = h.pipeline().snapshot().await.unwrap();
        assert!(state.attempts[&h.id].initial_grant.is_some());
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        return;
    }
    drop(joined);
    let state = h.pipeline().snapshot().await.unwrap();
    let source = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(source.complete() && source.verified.is_some());
    assert_eq!(source.operations.len(), 5);
    // A fresh second call must inspect the real owner again; a prior receipt
    // cannot silently replace either current retention or the actual clean HEAD.
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, service.client.clone())
        .await
        .unwrap();
    let handle = Uuid::parse_str(
        source.operations["import"].result.as_ref().unwrap()["retention_handle"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let current = service
        .client
        .query(
            source.binding().unwrap(),
            "retention.inspect",
            json!({"handle_id":handle,"limit":1}),
        )
        .unwrap();
    let release = crate::source::GitAction::Release {
        handle_id: handle,
        generation: current["generation"].as_u64().unwrap(),
        refs: serde_json::from_value(current["refs"].clone()).unwrap(),
    }
    .request(source.binding().unwrap(), Uuid::new_v4());
    service.client.exchange(&release).unwrap();
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, service.client.clone())
            .await
            .is_err()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_file_sync_and_cursor_commit_faults_reopen_old_or_matching_new_history() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let m = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), m.clone());
        h.mode.store(1, Ordering::SeqCst);
        let spec = h.spec().await;
        assert!(
            h.pipeline()
                .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
                .await
                .is_err()
        );
        let before = h.pipeline().snapshot().await.unwrap();
        let pending = before.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .pending
            .clone()
            .unwrap();
        let spec = h.spec().await;
        h.pipeline().test_journal_fault(fault).await.unwrap();
        h.mode.store(0, Ordering::SeqCst);
        assert!(
            h.pipeline()
                .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
                .await
                .is_err()
        );
        // File sync preceded the attempted metadata commit. Its exact tail is
        // retained even when the selected journal still has the old cursor.
        let file = git
            .client
            .root()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("download/manifest.json");
        assert_eq!(
            fs::metadata(&file).unwrap().len(),
            u64::from(pending.length)
        );
        drop(h.pipeline.take());
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        assert!(reopened.source_range(h.id).is_err());
        let s = reopened.state().attempts[&h.id].source.as_ref().unwrap();
        assert!(s.operations.is_empty());
        match fault {
            WriteFault::BeforeWrite | WriteFault::AfterFileSync => {
                assert_eq!(s.manifest_offset, 0);
                assert_eq!(
                    s.pending.as_ref().unwrap().operation_id,
                    pending.operation_id
                )
            }
            WriteFault::AfterRename | WriteFault::AfterDirectorySync => {
                assert_eq!(s.manifest_offset, u64::from(pending.length));
                assert!(s.pending.is_none())
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_source_read_allows_control_and_async_cancellation_retains_exact_pending_range() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.mode.store(2, Ordering::SeqCst);
    let spec = h.spec().await;
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation().clone();
    let local = git.client.clone();
    let id = h.id;
    let task =
        tokio::spawn(async move { pipeline.hydrate_source(id, preparation, spec, local).await });
    let before = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let s: State =
                serde_json::from_slice::<Snapshot>(&fs::read(h.root.join("journal.json")).unwrap())
                    .unwrap()
                    .state;
            if h.range_entered.load(Ordering::SeqCst) {
                break s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await
        }
    })
    .await
    .unwrap();
    let record = &before.attempts[&h.id];
    let pending = record
        .source
        .as_ref()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .operation_id;
    let control = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        h.client.accept(
            record.acceptance.as_ref().unwrap(),
            record.reservation.deadline_ns,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drop(control);
    task.abort();
    assert!(matches!(task.await, Err(e) if e.is_cancelled()));
    let after = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(state) => break state,
                Err(error) if error.to_string().contains("storage lane overloaded") => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await
                }
                Err(error) => panic!("unexpected retained owner error: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(after.attempts.len(), 1);
    let s = after.attempts[&h.id].source.as_ref().unwrap();
    assert_eq!(s.pending.as_ref().unwrap().operation_id, pending);
    assert_eq!(s.manifest_offset, 0);
    assert!(s.operations.is_empty());
    h.mode.store(0, Ordering::SeqCst);
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
        .await
        .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_source_geometry_refuses_before_any_range_or_git_mutation() {
    let mut export = synthetic();
    export.manifest.bundle_bytes = (1u64 << 30) + (1 << 20);
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let s = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(s.manifest.is_none() && s.pending.is_none() && s.operations.is_empty());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_native_inspection_capabilities_refuse_without_adopting_source_plan() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::profile(h.root.clone(), manifest, false);
    assert!(git.client.native_reservation_bytes().is_err());
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].source.is_none());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_retains_exact_request_and_requires_actual_inputs_again_for_retry() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation().clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    let renderer = before.attempts[&h.id]
        .reservation
        .offer
        .startup
        .renderer_qualification
        .clone();
    let prepared = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, source, renderer.clone())
        .await
        .unwrap();
    assert_eq!(prepared.request().journal_revision, before.revision);
    assert_eq!(
        prepared.request().journal_digest,
        canonical_digest(&before).unwrap()
    );
    assert_eq!(
        prepared.request().artifacts.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
            + 2
    );
    assert_eq!(
        prepared.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    let saved = h.pipeline().snapshot().await.unwrap();
    assert!(
        same(
            &saved.attempts[&h.id].prepared.as_ref().unwrap().request,
            prepared.request()
        )
        .unwrap()
    );
    let first_request = prepared.request().clone();
    drop(prepared);
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation().clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let retry = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, source, renderer)
        .await
        .unwrap();
    assert!(same(&first_request, retry.request()).unwrap());
    assert_eq!(retry.request().journal_revision, before.revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_refuses_missing_inputs_renderer_and_retention_changed_after_hydration() {
    for mode in 0..5 {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let mut all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation().clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        let mut renderer = before.attempts[&h.id]
            .reservation
            .offer
            .startup
            .renderer_qualification
            .clone();
        match mode {
            0 => {
                all.pop();
            }
            1 => renderer.digest = Digest::of(b"uninstalled renderer"),
            2 => git.mode.store(2, Ordering::SeqCst),
            3 => git.mode.store(3, Ordering::SeqCst),
            _ => {
                let lease = h.preparation().plan().materials[0].lease_id;
                fs::write(
                    h._dir
                        .path()
                        .join("materials")
                        .join(lease.to_string())
                        .join("material.bin"),
                    b"corrupt installed key",
                )
                .unwrap();
            }
        }
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation().clone(), all, source, renderer)
                .await
                .is_err(),
            "mode {mode}"
        );
        let after = h.pipeline().snapshot().await.unwrap();
        assert!(same(&before, &after).unwrap());
        assert!(after.attempts[&h.id].prepared.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_fsync_cuts_retain_original_or_complete_intent_and_reopen_without_authority() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation().clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        let renderer = before.attempts[&h.id]
            .reservation
            .offer
            .startup
            .renderer_qualification
            .clone();
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation().clone(), all, source, renderer)
                .await
                .is_err()
        );
        assert!(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .reservation
                .disk_bytes
                > 0
        );
        drop(h.pipeline.take());
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        if let Some(p) = &reopened.state().attempts[&h.id].prepared {
            assert_eq!(p.request.journal_revision, before.revision);
            assert_eq!(p.request.journal_digest, canonical_digest(&before).unwrap());
        } else {
            assert!(same(reopened.state(), &before).unwrap());
        }
    }
}

#[test]
fn format_three_refusal_preserves_old_journal_and_pending_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old-three");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 3);
    let original = fs::read(root.join("journal.json")).unwrap();
    fs::write(root.join("pending"), b"unknown old write").unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), original);
    assert_eq!(
        fs::read(root.join("pending")).unwrap(),
        b"unknown old write"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_overlaps_only_two_reads_and_preserves_control_capacity() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation().clone();
    let id = h.id;
    let job = tokio::spawn(async move { pipeline.artifacts(id, preparation).await });
    // Both requests must reach the independently blocked HTTP peer. Sequential
    // fetching cannot satisfy this barrier; no timing benchmark is involved.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.inline_entered.acquire_many(2),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    let pending = h.pipeline().snapshot().await.unwrap();
    assert_eq!(pending.revision, before.revision + 1);
    let attempt = &pending.attempts[&h.id];
    assert!(attempt.downloads.values().all(|d| d.receipt.is_none()));
    // Both existing bulk slots are occupied, but authenticated control still
    // completes on its independent lane. Replaying does not extend the cap.
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.client.accept(
            attempt.acceptance.as_ref().unwrap(),
            attempt.reservation.deadline_ns,
            Some(h.preparation()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(replay.deadline_ns() <= h.preparation().deadline_ns());
    let requests: usize = attempt.downloads.values().map(|d| d.requests.len()).sum();
    h.inline_release.add_permits(requests);
    let inputs = job.await.unwrap().unwrap();
    assert_eq!(inputs.len(), attempt.downloads.len());
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.pipeline().snapshot().await.unwrap().revision,
        before.revision + 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_uses_two_durable_commits_and_replay_reuses_every_original_request() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    let first = h
        .pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    let saved = h.pipeline().snapshot().await.unwrap();
    assert_eq!(saved.revision, before.revision + 2);
    assert_eq!(
        first.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    for op in saved.attempts[&h.id].downloads.values() {
        assert_eq!(op.created_revision, before.revision);
        assert!(op.receipt.is_some() && op.completed_ns.is_some());
    }
    let second = h
        .pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    assert_eq!(second.len(), first.len());
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.bytes(), b.bytes());
    }
    assert!(same(&saved, &h.pipeline().snapshot().await.unwrap()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_inline_batch_keeps_exact_plan_without_partial_receipt_completion() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(3, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .artifacts(h.id, h.preparation().clone())
            .await
            .is_err()
    );
    let partial = h.pipeline().snapshot().await.unwrap();
    assert_eq!(partial.revision, before.revision + 1);
    let plans = &partial.attempts[&h.id].downloads;
    assert_eq!(
        plans.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    assert!(
        plans
            .values()
            .all(|p| p.receipt.is_none() && p.completed_ns.is_none())
    );
    h.mode.store(0, Ordering::SeqCst);
    h.pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    let complete = h.pipeline().snapshot().await.unwrap();
    assert_eq!(complete.revision, before.revision + 2);
    for (key, p) in plans {
        let actual = &complete.attempts[&h.id].downloads[key];
        assert!(same(&p.requests, &actual.requests).unwrap());
        assert_eq!(p.request_digests, actual.request_digests);
        assert_eq!(p.created_revision, actual.created_revision);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_fsync_cuts_never_reopen_partial_plan_or_completion() {
    for completion in [false, true] {
        for fault in [
            WriteFault::BeforeWrite,
            WriteFault::AfterFileSync,
            WriteFault::AfterRename,
            WriteFault::AfterDirectorySync,
        ] {
            let mut h = Inputs::new(synthetic()).await;
            if completion {
                h.mode.store(3, Ordering::SeqCst);
                assert!(
                    h.pipeline()
                        .artifacts(h.id, h.preparation().clone())
                        .await
                        .is_err()
                );
                h.mode.store(0, Ordering::SeqCst);
            }
            let before = h.pipeline().snapshot().await.unwrap();
            h.pipeline().test_journal_fault(fault).await.unwrap();
            assert!(
                h.pipeline()
                    .artifacts(h.id, h.preparation().clone())
                    .await
                    .is_err()
            );
            drop(h.pipeline.take());
            let mut capacity = limits();
            capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
            let old = Journal::open(&h.root, identity().node_id, capacity).unwrap();
            assert!(old.available().is_err());
            let a = &old.state().attempts[&h.id];
            let count = a.reservation.offer.offered.artifacts.len();
            assert!(a.downloads.is_empty() || a.downloads.len() == count);
            let complete = a.downloads.values().filter(|d| d.receipt.is_some()).count();
            assert!(complete == 0 || complete == count);
            assert!(
                old.state().revision == before.revision
                    || old.state().revision == before.revision + 1
            );
            if completion {
                for (key, op) in &before.attempts[&h.id].downloads {
                    assert!(same(&op.requests, &a.downloads[key].requests).unwrap());
                }
            }
        }
    }
}

#[path = "granted_scenarios.rs"]
mod granted_scenarios;

#[path = "presence_scenarios.rs"]
pub(super) mod presence_scenarios;

async fn phase_physical(
    h: &Inputs,
    git: GitClient,
) -> (
    Arc<crate::native::http::preparation_v2::AcceptedPreparation>,
    Vec<crate::native::VerifiedArtifact>,
    crate::native::preparation::HydratedWorkspace,
    ArtifactRef,
) {
    let accepted = h.phase.as_ref().unwrap().clone();
    let before = h.pipeline().snapshot().await.unwrap();
    let a = &before.attempts[&h.id];
    let cap = a.reservation.deadline_ns;
    crate::native::http::test_clock::expire(cap).await;
    let spec = h
        .pipeline()
        .artifact_phase(h.id, accepted.clone(), h.reference.clone())
        .await
        .unwrap();
    let workspace = h
        .pipeline()
        .hydrate_source_phase(h.id, accepted.clone(), spec, git)
        .await
        .unwrap();
    assert!(workspace.observed_ns() > cap);
    let all = h
        .pipeline()
        .artifacts_phase(h.id, accepted.clone())
        .await
        .unwrap();
    for lease in &accepted.delivery().receipt.lease.materials {
        h.pipeline()
            .material_phase(h.id, accepted.clone(), lease.lease_id)
            .await
            .unwrap();
    }
    let renderer = a.reservation.offer.startup.renderer_qualification.clone();
    (accepted, all, workspace, renderer)
}
async fn phase_join(h: &Inputs, git: GitClient) -> crate::native::preparation::PreparedPhaseInputs {
    let (accepted, all, workspace, renderer) = phase_physical(h, git).await;
    let joined = h
        .pipeline()
        .prepare_inputs_phase(h.id, accepted.clone(), all, workspace, renderer)
        .await
        .unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    joined
        .request()
        .validate_for(
            &accepted.delivery().receipt,
            &a.phase_acceptance.as_ref().unwrap().request,
            &a.phase_acceptance.as_ref().unwrap().proposal,
        )
        .unwrap();
    assert!(a.preparation.is_none() && a.prepared.is_none() && a.initial_grant.is_none());
    assert!(a.phase_prepared.is_some());
    joined
}
#[tokio::test]
async fn phase_inputs_collect_real_bytes_and_git_after_original_proposal_expiry() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let joined = phase_join(&h, git.client.clone()).await;
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_phase_inputs_and_prepare_after_original_proposal_expiry() {
    actual_git_hydration(false, true).await;
}

async fn shorter_phase(
    h: &Inputs,
    ms: u64,
) -> Arc<crate::native::http::preparation_v2::AcceptedPreparation> {
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    let p = h
        .client
        .propose_preparation(
            &identity(),
            &a.reservation.offer,
            &intent.proposal.negotiation,
            a.reservation.deadline_ns,
        )
        .await
        .unwrap();
    h.phase_window.store(ms, Ordering::SeqCst);
    Arc::new(
        h.client
            .accept_preparation(&p, &intent.request, h.phase.as_deref())
            .await
            .unwrap(),
    )
}
#[tokio::test]
async fn phase_input_expiry_and_foreign_observation_refuse_without_intents() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let h = Inputs::new_phase(synthetic()).await;
    let short = shorter_phase(&h, 50).await;
    // Acceptance succeeds before the cap even if HTTP is delayed by shared load.
    // Refuse at the exact original deadline, without granting a fresh interval.
    clock.advance_to(short.deadline_ns());
    assert!(
        h.pipeline()
            .artifacts_phase(h.id, short.clone())
            .await
            .is_err()
    );
    let lease = short.delivery().receipt.lease.materials[0].lease_id;
    assert!(
        h.pipeline()
            .material_phase(h.id, short, lease)
            .await
            .is_err()
    );
    let other = Inputs::new_phase(synthetic()).await;
    assert!(
        h.pipeline()
            .artifacts_phase(h.id, other.phase.as_ref().unwrap().clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    assert!(a.downloads.is_empty() && a.material_downloads.is_empty() && a.source.is_none());
    assert_eq!(state.attempts.len(), 1);
}
#[tokio::test]
async fn phase_cancelled_artifact_batch_keeps_exact_ids_and_no_partial_completion() {
    let h = Inputs::new_phase(synthetic()).await;
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let a = h.phase.as_ref().unwrap().clone();
    let id = h.id;
    let task = tokio::spawn(async move { pipeline.artifacts_phase(id, a).await });
    h.inline_entered.acquire_many(2).await.unwrap().forget();
    task.abort();
    assert!(matches!(task.await,Err(error) if error.is_cancelled()));
    let before = h.pipeline().snapshot().await.unwrap();
    let original = before.attempts[&id].downloads.clone();
    assert!(original.values().all(|d| d.receipt.is_none()));
    h.mode.store(0, Ordering::SeqCst);
    h.inline_release.add_permits(2);
    h.pipeline()
        .artifacts_phase(id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    let after = h.pipeline().snapshot().await.unwrap();
    for (key, old) in original {
        let new = &after.attempts[&id].downloads[&key];
        assert_eq!(
            canonical_bytes(&old.requests).unwrap(),
            canonical_bytes(&new.requests).unwrap()
        );
        assert!(new.receipt.is_some());
    }
}
#[tokio::test]
async fn phase_input_write_cuts_hold_old_or_complete_plan_and_history_only_reopen() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let mut h = Inputs::new_phase(synthetic()).await;
        let a = h.phase.as_ref().unwrap().clone();
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(h.pipeline().artifacts_phase(h.id, a.clone()).await.is_err());
        drop(h.pipeline.take());
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
        assert!(reopened.available().is_err());
        assert!(reopened.check_phase_preparation(h.id, &a).is_err());
        let attempt = &reopened.state().attempts[&h.id];
        assert!(attempt.preparation.is_none() && attempt.initial_grant.is_none());
        assert!(
            attempt.downloads.is_empty()
                || attempt.downloads.len() == attempt.reservation.offer.offered.artifacts.len()
        );
        assert!(
            attempt
                .downloads
                .values()
                .all(|d| matches!(d.preparation, InputOrigin::Phase(_)) && d.receipt.is_none())
        );
    }
}
#[tokio::test]
async fn phase_origin_tampering_cannot_be_hidden_by_valid_journal_checksum() {
    let mut h = Inputs::new_phase(synthetic()).await;
    h.pipeline()
        .artifacts_phase(h.id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    drop(h.pipeline.take());
    rewrite(&h.root, |s| {
        let op = s
            .attempts
            .get_mut(&h.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap();
        let InputOrigin::Phase(p) = &mut op.preparation else {
            panic!()
        };
        p.receipt_digest = Digest::of(b"changed accepted receipt");
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&h.root, identity().node_id, capacity).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_prepare_cancelled_fsync_retains_exact_request_and_replay_does_not_retime() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let (a, all, workspace, renderer) = phase_physical(&h, git.client.clone()).await;
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let id = h.id;
    let task = tokio::spawn(async move {
        pipeline
            .prepare_inputs_phase(id, a, all, workspace, renderer)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await
        }
    })
    .await
    .unwrap();
    task.abort();
    pause.release();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    // Snapshot must wait for the admitted fsync owner, not an arbitrary sleep.
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(s) => break s,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    let original = first.attempts[&id].phase_prepared.clone().unwrap();
    let joined = phase_join(&h, git.client.clone()).await;
    assert_eq!(
        canonical_bytes(joined.request()).unwrap(),
        canonical_bytes(&original.request).unwrap()
    );
    let last = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        canonical_bytes(last.attempts[&id].phase_prepared.as_ref().unwrap()).unwrap(),
        canonical_bytes(&original).unwrap()
    );
    assert!(last.attempts[&id].initial_grant.is_none());
}
#[tokio::test]
async fn phase_shared_malformed_owner_is_rejected_even_when_every_input_copy_matches() {
    let mut h = Inputs::new_phase(synthetic()).await;
    h.pipeline()
        .artifacts_phase(h.id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    drop(h.pipeline.take());
    rewrite(&h.root, |state| {
        let a = state.attempts.get_mut(&h.id).unwrap();
        let r = a.phase_preparation.as_mut().unwrap();
        r.delivery.receipt.lease.choice_digest = Digest::of(b"forged shared owner");
        r.receipt_digest = r.delivery.receipt.digest().unwrap();
        for op in a.downloads.values_mut() {
            op.preparation = InputOrigin::Phase(Box::new(r.clone()));
        }
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&h.root, identity().node_id, capacity).is_err());
}

#[tokio::test]
async fn phase_and_legacy_input_provenance_cannot_cross_on_reopen() {
    let mut phase = Inputs::new_phase(synthetic()).await;
    phase
        .pipeline()
        .artifacts_phase(phase.id, phase.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    let mut legacy = Inputs::new(synthetic()).await;
    let _verified = legacy.spec().await;
    let phase_state = phase.pipeline().snapshot().await.unwrap();
    let phase_record = phase_state.attempts[&phase.id]
        .phase_preparation
        .as_ref()
        .unwrap()
        .clone();
    let legacy_state = legacy.pipeline().snapshot().await.unwrap();
    let legacy_record = legacy_state.attempts[&legacy.id]
        .preparation
        .as_ref()
        .unwrap()
        .clone();
    drop(phase.pipeline.take());
    drop(legacy.pipeline.take());
    rewrite(&phase.root, |s| {
        s.attempts
            .get_mut(&phase.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap()
            .preparation = InputOrigin::Legacy(Box::new(legacy_record));
    });
    rewrite(&legacy.root, |s| {
        s.attempts
            .get_mut(&legacy.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap()
            .preparation = InputOrigin::Phase(Box::new(phase_record));
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&phase.root, identity().node_id, capacity.clone()).is_err());
    assert!(Journal::open(&legacy.root, identity().node_id, capacity).is_err());
}
#[tokio::test]
async fn phase_prepare_rename_cut_retains_whole_intent_without_live_owner_on_reopen() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let (a, all, workspace, renderer) = phase_physical(&h, git.client.clone()).await;
    h.pipeline()
        .test_journal_fault(WriteFault::AfterRename)
        .await
        .unwrap();
    assert!(
        h.pipeline()
            .prepare_inputs_phase(h.id, a.clone(), all, workspace, renderer)
            .await
            .is_err()
    );
    drop(h.pipeline.take());
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
    assert!(reopened.available().is_err());
    assert!(reopened.check_phase_preparation(h.id, &a).is_err());
    let attempt = &reopened.state().attempts[&h.id];
    assert!(
        attempt.phase_prepared.is_some()
            && attempt.prepared.is_none()
            && attempt.initial_grant.is_none()
    );
    assert!(attempt.source.as_ref().unwrap().complete());
}

#[path = "phase_execution_scenarios.rs"]
mod phase_execution_scenarios;

#[path = "native_host_scenarios.rs"]
mod native_host_scenarios;

#[tokio::test]
async fn production_collect_dispatch_retains_actual_frozen_inputs_without_launch() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_collect(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    assert!(a.phase_acceptance.is_none() && a.refresh_acceptance.is_none());
    let (_config_root, mut config) = crate::native::runner::tests::config();
    let published = published_phase_fixture();
    config.negotiation =
        serde_json::from_value(published["proposal"]["negotiation"].clone()).unwrap();
    config.capabilities = capabilities();
    config.source_profile =
        serde_json::from_value(published["accept"]["hold"]["source_profile"].clone()).unwrap();
    config.maximum_preparation_ms = 30000;
    config.renderer = a.reservation.offer.startup.renderer_qualification.clone();
    let grant = crate::native::runner::test_collect(
        h.pipeline(),
        &h.inventory().await,
        &config,
        h.id,
        &a.reservation.offer,
        a.reservation.deadline_ns,
        git.client.clone(),
    )
    .await
    .unwrap();
    h.pipeline().check_phase_execution(&grant).await.unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].phase_execution.is_some());
    assert!(state.attempts[&h.id].native_host.is_none());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
