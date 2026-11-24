fn download_fixture() -> (
    Peer,
    TempDir,
    Runtime<FakeClock>,
    Uuid,
    TransferManifest,
    Vec<u8>,
    Vec<u8>,
) {
    use sha2::{Digest, Sha256};
    let peer = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    let source = o.spec.bindings.as_ref().unwrap().source.clone().unwrap();
    let bundle = vec![b'p'; 600_000];
    let artifact = Uuid::from_u128(201);
    let native = aq_worker::source::GitManifest {
        format_version: 1,
        repository_id: source.repository_id,
        repository_identity: source.logical_identity.clone(),
        binding_generation: source.binding_generation,
        transfer_id: artifact,
        object_format: "sha1".into(),
        base: source.base.clone(),
        source: source.base.clone(),
        prerequisites: vec![],
        bundle_size: bundle.len() as u64,
        bundle_sha256: Some(format!("{:x}", Sha256::digest(&bundle))),
        producer_installation_id: Uuid::from_u128(202),
        reference_only: false,
        caller_correlation: Value::Null,
    };
    let bytes = serde_json::to_vec(&native).unwrap();
    let manifest = TransferManifest {
        transfer_id: Uuid::from_u128(203),
        role: SourceRole::Base,
        checkpoint: source.base.clone(),
        attempt: o.attempt.clone(),
        resolved_spec_digest: o.spec.resolved_spec_digest.clone(),
        source,
        artifact_id: artifact,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        bundle_digest: native.bundle_sha256,
        manifest_bytes: bytes.len() as u64,
        bundle_bytes: bundle.len() as u64,
    };
    authorize(&mut rt, &peer, o);
    (peer, temp, rt, id, manifest, bytes, bundle)
}

#[test]
fn download_restart_replays_only_unacknowledged_tail_and_cannot_grant_import() {
    use aq_worker::source_transfer::DownloadAction;
    let (peer, temp, mut rt, id, manifest, bytes, bundle) = download_fixture();
    let before = rt.journal.state().attempts[&id].pending_bytes;
    rt.prepare_base_download(id, manifest.clone(), &peer.client)
        .unwrap();
    assert_eq!(
        rt.journal.state().attempts[&id].pending_bytes,
        before + manifest.manifest_bytes + manifest.bundle_bytes
    );
    assert!(rt.prepare_source(id, &peer.client).is_err());
    let range = match rt.next_base_download(id).unwrap().unwrap() {
        DownloadAction::Range(r) => r,
        _ => unreachable!(),
    };
    rt.journal
        .inject_write_fault(aq_worker::journal::WriteFault::AfterFileSync);
    assert!(rt.store_base_download_chunk(id, &range, &bytes).is_err());
    let path = peer
        .client
        .root()
        .join("artifacts/incoming")
        .join(manifest.artifact_id.to_string())
        .join("manifest.json");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(rt);
    let mut identity = sample_offer().identity;
    identity.boot_id = Uuid::new_v4();
    let mut rt = Runtime::new(
        Journal::open(temp.path(), identity, limits()).unwrap(),
        FakeClock::new(200),
    );
    let replay = match rt.next_base_download(id).unwrap().unwrap() {
        DownloadAction::Range(r) => r,
        _ => unreachable!(),
    };
    assert_eq!(range, replay);
    rt.store_base_download_chunk(id, &replay, &bytes).unwrap();
    finish_bundle_download(&mut rt, id, &bundle);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_download_without_workspace(&mut rt, id, &manifest, &peer);
}
fn finish_bundle_download(rt: &mut Runtime<FakeClock>, id: Uuid, bundle: &[u8]) {
    use aq_worker::source_transfer::DownloadAction;
    loop {
        match rt.next_base_download(id).unwrap() {
            Some(DownloadAction::Range(range)) => {
                assert_eq!(range.file, TransferFile::Bundle);
                let start = range.offset as usize;
                rt.store_base_download_chunk(id, &range, &bundle[start..start + range.length])
                    .unwrap();
            }
            Some(DownloadAction::Verify(v)) => rt
                .acknowledge_base_verification(id, v.verify().unwrap())
                .unwrap(),
            None => break,
        }
    }
}

fn assert_download_without_workspace(
    rt: &mut Runtime<FakeClock>,
    id: Uuid,
    manifest: &TransferManifest,
    peer: &Peer,
) {
    assert!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .download
            .as_ref()
            .unwrap()
            .verified
    );
    assert!(
        rt.import_base(id, manifest.artifact_id, &peer.client)
            .unwrap_err()
            .to_string()
            .contains("incarnation")
    );
    assert!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .workspace
            .is_none()
    );
}

#[test]
fn download_refuses_changed_assignment_or_corrupt_complete_bytes() {
    use aq_worker::source_transfer::DownloadAction;
    let (peer, _temp, mut rt, id, manifest, bytes, bundle) = download_fixture();
    rt.prepare_base_download(id, manifest.clone(), &peer.client)
        .unwrap();
    let mut changed = manifest.clone();
    changed.transfer_id = Uuid::new_v4();
    assert!(rt.prepare_base_download(id, changed, &peer.client).is_err());
    let verification = loop {
        match rt.next_base_download(id).unwrap().unwrap() {
            DownloadAction::Range(range) => {
                let data = if range.file == TransferFile::Manifest {
                    &bytes
                } else {
                    &bundle
                };
                let start = range.offset as usize;
                rt.store_base_download_chunk(id, &range, &data[start..start + range.length])
                    .unwrap();
            }
            DownloadAction::Verify(v) => break v,
        }
    };
    let path = peer
        .client
        .root()
        .join("artifacts/incoming")
        .join(manifest.artifact_id.to_string())
        .join("objects.bundle");
    std::fs::write(path, vec![b'x'; bundle.len()]).unwrap();
    assert!(
        verification
            .verify()
            .err()
            .unwrap()
            .to_string()
            .contains("digest")
    );
    assert!(
        !rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .download
            .as_ref()
            .unwrap()
            .verified
    );
    assert!(
        rt.import_base(id, manifest.artifact_id, &peer.client)
            .unwrap_err()
            .to_string()
            .contains("verified")
    );
}

#[test]
fn download_reservation_pressure_preserves_existing_source_and_allocates_no_files() {
    let (peer, _temp, mut rt, id, manifest, _bytes, _bundle) = download_fixture();
    let pending = rt.journal.limits().max_pending_bytes;
    rt.journal
        .transaction(|s| {
            s.attempts.get_mut(&id).unwrap().pending_bytes = pending;
            Ok(())
        })
        .unwrap();
    assert!(
        rt.prepare_base_download(id, manifest.clone(), &peer.client)
            .is_err()
    );
    let source = rt.journal.state().attempts[&id].source.as_ref().unwrap();
    assert!(source.download.is_none());
    assert_eq!(rt.journal.state().attempts[&id].pending_bytes, pending);
    assert!(
        !peer
            .client
            .root()
            .join("artifacts/incoming")
            .join(manifest.artifact_id.to_string())
            .exists()
    );
}

#[test]
fn download_nonstart_cleanup_requires_release_and_preserves_unknown_git_effects() {
    use aq_worker::source_transfer::DownloadAction;
    for pending in [false, true] {
        let (peer, _temp, mut rt, id, manifest, bytes, _bundle) = download_fixture();
        rt.prepare_base_download(id, manifest.clone(), &peer.client)
            .unwrap();
        let range = match rt.next_base_download(id).unwrap().unwrap() {
            DownloadAction::Range(r) => r,
            _ => unreachable!(),
        };
        rt.store_base_download_chunk(id, &range, &bytes).unwrap();
        let directory = peer
            .client
            .root()
            .join("artifacts/incoming")
            .join(manifest.artifact_id.to_string());
        assert!(rt.retire_source(id, &peer.client).is_err());
        assert!(directory.exists());
        if pending {
            rt.journal.transaction(|s|{let source=s.attempts.get_mut(&id).unwrap().source.as_mut().unwrap();let op=Uuid::new_v4();source.operations.insert("import-base-pending".into(),aq_worker::source::PendingGitOperation{operation_id:op,request:json!({"operation_id":op,"repository_id":source.binding.repository_id}),result:None});Ok(())}).unwrap();
        }
        let report = rt
            .report_not_started(id, ProcessOutcome::Cancelled)
            .unwrap();
        rt.acknowledge_result(ResultResponse {
            report_id: report.report_id,
            disposition: ResultDisposition::Accepted,
            retention_release: true,
        })
        .unwrap();
        if pending {
            assert!(rt.retire_source(id, &peer.client).is_err());
            assert_eq!(
                std::fs::read(directory.join("manifest.json")).unwrap(),
                bytes
            );
        } else {
            std::fs::write(directory.join("unknown"), b"preserve").unwrap();
            assert!(rt.retire_source(id, &peer.client).is_err());
            assert_eq!(
                std::fs::read(directory.join("unknown")).unwrap(),
                b"preserve"
            );
            std::fs::remove_file(directory.join("unknown")).unwrap();
            rt.retire_source(id, &peer.client).unwrap();
            rt.retire(id).unwrap();
            assert!(!directory.exists());
            assert_eq!(rt.journal.state().attempts[&id].pending_bytes, 0);
            assert!(
                rt.journal.state().attempts[&id]
                    .source
                    .as_ref()
                    .unwrap()
                    .receipt
                    .is_none()
            );
        }
    }
}
