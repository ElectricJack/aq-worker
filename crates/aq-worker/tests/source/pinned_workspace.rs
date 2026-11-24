#[test]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; no sibling checkout"]
fn pinned_git_real_workspace_checkpoint_export_and_feature_refusal() {
    let exe = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set pinned executable");
    let exe = Path::new(&exe);
    let temp = TempDir::new().unwrap();
    let fixture = source_repository::create_fixture(exe, temp.path());
    let (source, mut rt) = source_repository::PreparedSource::new(&fixture, temp.path());
    let git = &source.guard.client;
    let id = source.id;
    assert_checkpoint_author(&rt, &source);
    let out = rt.export_source(id, git).unwrap();
    let manifest: aq_worker::source::GitManifest =
        serde_json::from_value(out["manifest"].clone()).unwrap();
    assert_eq!(manifest.source, source.checkpoint.source);
    assert!(rt.journal.state().attempts[&id].pending_bytes > 0);
    assert!(rt.retire_source(id, git).is_err());
    import_result_bundle(&fixture, &source, &manifest);
    let receipt = bind_result(&mut rt, &source, &manifest);
    release_result(&mut rt, &source, receipt);
    verify_fresh_attempt(&mut rt, &source, &fixture, &manifest);
    let worker = source.worker.clone();
    drop(source.guard);
    binary_source_fixture(
        &fixture,
        temp.path(),
        &worker,
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
    );
}
fn import_result_bundle(
    fixture: &source_repository::Fixture,
    source: &source_repository::PreparedSource,
    manifest: &aq_worker::source::GitManifest,
) {
    let base = &fixture.binding.base.hex;
    let worker = &source.worker;
    // The result bundle names the issued base as its prerequisite and carries
    // only what follows it; the central repository, which holds that base,
    // imports and retains it.
    assert_eq!(manifest.prerequisites, vec![manifest.base.clone()]);
    let result_handle = manifest.transfer_id.to_string();
    let header = std::fs::read(
        worker
            .join("artifacts")
            .join(&result_handle)
            .join("objects.bundle"),
    )
    .unwrap();
    assert!(
        String::from_utf8_lossy(&header[..header.len().min(512)]).contains(&format!("-{base}")),
        "bundle header lacks the base prerequisite"
    );
    source_repository::copy_artifact(worker, &fixture.central, &result_handle);
    let retained = fixture.central_request(
        "artifact.import",
        json!({"artifact_handle":result_handle}),
        1,
    );
    assert_eq!(retained["retention_state"], "active");
    assert_eq!(
        retained["source"],
        serde_json::to_value(&manifest.source).unwrap()
    );
}

fn bind_result(
    rt: &mut Runtime<FakeClock>,
    source: &source_repository::PreparedSource,
    manifest: &aq_worker::source::GitManifest,
) -> CentralRetentionReceipt {
    use sha2::{Digest, Sha256};
    let id = source.id;
    let worker = &source.worker;
    let manifest_file = std::fs::read(
        worker
            .join("artifacts")
            .join(manifest.transfer_id.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    let transfer = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint: manifest.source.clone(),
        attempt: rt.journal.state().attempts[&id].offer.attempt.clone(),
        resolved_spec_digest: rt.journal.state().attempts[&id]
            .offer
            .spec
            .resolved_spec_digest
            .clone(),
        source: rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
        artifact_id: manifest.transfer_id,
        manifest_digest: format!("{:x}", Sha256::digest(&manifest_file)),
        bundle_digest: manifest.bundle_sha256.clone(),
        manifest_bytes: manifest_file.len() as u64,
        bundle_bytes: manifest.bundle_size,
    };
    rt.bind_source_transfer(id, transfer.clone()).unwrap();
    CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: transfer.transfer_id,
        repository_id: transfer.source.repository_id,
        binding_generation: 1,
        source: source.checkpoint.source.clone(),
        retention_handle: manifest.transfer_id,
        manifest_digest: manifest.canonical_digest().unwrap(),
    }
}

fn release_result(
    rt: &mut Runtime<FakeClock>,
    source: &source_repository::PreparedSource,
    receipt: CentralRetentionReceipt,
) {
    let id = source.id;
    let git = &source.guard.client;
    let mut wrong = receipt.clone();
    wrong.transfer_id = Uuid::new_v4();
    assert!(rt.acknowledge_source(id, wrong).is_err());
    let mut wrong = receipt.clone();
    wrong.manifest_digest = "0".repeat(64);
    assert!(rt.acknowledge_source(id, wrong).is_err());
    rt.acknowledge_source(id, receipt).unwrap();
    let report = rt.journal.state().attempts[&id].report.clone().unwrap();
    rt.acknowledge_result(ResultResponse {
        report_id: report.report_id,
        disposition: ResultDisposition::Accepted,
        retention_release: true,
    })
    .unwrap();
    rt.retire_source(id, git).unwrap();
    rt.retire(id).unwrap();
    assert!(!source.work.path.exists());
    rt.reclaim_source(id, 1, git).unwrap();
    assert_eq!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .operations["evict"]
            .result
            .as_ref()
            .unwrap()["lifecycle"],
        "absent"
    );
}

fn verify_fresh_attempt(
    rt: &mut Runtime<FakeClock>,
    source: &source_repository::PreparedSource,
    fixture: &source_repository::Fixture,
    manifest: &aq_worker::source::GitManifest,
) {
    let git = &source.guard.client;
    let id = source.id;
    let worker = &source.worker;
    // A new independent attempt is permitted only through explicit frozen inputs.
    let mut next = offer();
    next.spec.project_id = Uuid::new_v4();
    next.offer_id = Uuid::new_v4();
    next.attempt.attempt_id = Uuid::new_v4();
    next.attempt.session_id = Uuid::new_v4();
    next.spec.bindings.as_mut().unwrap().source = Some(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding
            .clone(),
    );
    next.spec = next.spec.seal();
    let next_id = next.attempt.attempt_id;
    let mut peer = FakeCoordinator::new(next.clone(), 100);
    let a = rt.reserve_source(next, git).unwrap();
    rt.authorize(peer.accept(&a).unwrap().grant, 100).unwrap();
    let artifact = fixture.exported["artifact_handle"].as_str().unwrap();
    source_repository::copy_artifact(&fixture.central, worker, artifact);
    assert!(
        rt.import_base(next_id, Uuid::parse_str(artifact).unwrap(), git)
            .is_err()
    );
    assert_eq!(
        rt.journal.state().attempts[&next_id]
            .source
            .as_ref()
            .unwrap()
            .operations[&format!("import-base-{artifact}")]
            .result
            .as_ref()
            .unwrap()["retention_state"],
        "released"
    );
    let fresh = fixture.central_request("artifact.export",
        json!({"base":manifest.base,"source":fixture.checkpoint["source"],"retention_handle":fixture.checkpoint["retention_handle"]}),
        1,
    );
    let fresh_handle = fresh["artifact_handle"].as_str().unwrap();
    source_repository::copy_artifact(&fixture.central, worker, fresh_handle);
    rt.import_base(next_id, Uuid::parse_str(fresh_handle).unwrap(), git)
        .unwrap();
    let second = rt.prepare_source(next_id, git).unwrap();
    assert_ne!(second.path, source.work.path);
    assert!(!second.path.join("result.txt").exists());
    std::fs::write(second.path.join(".gitattributes"), "* text\n").unwrap();
    let h = rt
        .launch_fixture(next_id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(next_id, h).unwrap();
    assert!(rt.checkpoint_source(next_id, git).is_err());
    assert!(rt.reclaim_source(id, 1, git).is_err());
}
