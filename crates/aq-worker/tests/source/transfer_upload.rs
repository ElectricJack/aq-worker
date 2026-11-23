use aq_worker::source_transfer::UploadAction;
fn accept_upload_action(
    rt: &mut Runtime<FakeClock>,
    id: Uuid,
    action: &aq_worker::source_transfer::UploadAction,
) {
    use aq_worker::source_transfer::UploadAction;
    let mut status = upload_status(rt, id);
    match action {
        UploadAction::Begin { .. } => {}
        UploadAction::Chunk { request, .. } => match request.file {
            TransferFile::Manifest => status.manifest_offset += u64::from(request.length),
            TransferFile::Bundle => status.bundle_offset += u64::from(request.length),
        },
        UploadAction::Finalize(_) => panic!("test must provide actual matching receipt"),
    }
    rt.acknowledge_source_upload(id, action, status).unwrap();
}

#[test]
fn upload_restart_preserves_intent_chunk_and_finalize_ids_until_exact_retention() {
    let (peer, temp, rt, id) = upload_fixture();
    let recovery = UploadRecovery { peer, temp, id };
    let (rt, transfer) = recovery.replay_begin(rt);
    let rt = recovery.replay_lost_chunk(rt);
    let (mut rt, finalize) = recovery.replay_finalize(rt);
    let receipt = recovery.acknowledge_receipt(&mut rt, &finalize, transfer);
    recovery.replay_report(rt, receipt);
}
struct UploadRecovery {
    peer: Peer,
    temp: TempDir,
    id: Uuid,
}
impl UploadRecovery {
    fn replay_begin(&self, mut rt: Runtime<FakeClock>) -> (Runtime<FakeClock>, TransferManifest) {
        let id = self.id;
        let begin = rt.next_source_upload(id).unwrap().unwrap();
        let (begin_id, transfer) = match &begin {
            UploadAction::Begin {
                operation_id,
                manifest,
                ..
            } => (*operation_id, manifest.clone()),
            _ => panic!("begin expected"),
        };
        assert!(
            !rt.journal.state().attempts[&id]
                .report
                .as_ref()
                .unwrap()
                .claimed_success
        );
        drop(rt);
        let mut identity = sample_offer().identity;
        identity.boot_id = Uuid::new_v4();
        let mut rt = Runtime::new(
            Journal::open(self.temp.path(), identity.clone(), limits()).unwrap(),
            FakeClock::new(200),
        );
        rt.prepare_source_upload(id, &self.peer.client).unwrap();
        let replay = rt.next_source_upload(id).unwrap().unwrap();
        match &replay {
            UploadAction::Begin {
                operation_id,
                manifest,
                ..
            } => {
                assert_eq!(*operation_id, begin_id);
                assert_eq!(manifest, &transfer);
            }
            _ => panic!("same begin expected"),
        }
        accept_upload_action(&mut rt, id, &replay);
        (rt, transfer)
    }

    fn replay_lost_chunk(&self, mut rt: Runtime<FakeClock>) -> Runtime<FakeClock> {
        let id = self.id;
        let action = rt.next_source_upload(id).unwrap().unwrap();
        let pending = match &action {
            UploadAction::Chunk { request, .. } => request.clone(),
            _ => panic!("chunk expected"),
        };
        // The peer could commit this chunk and lose its response. No local progress
        // is acknowledged; restart must retransmit precisely the same operation.
        let mut identity = rt.journal.state().identity.clone();
        drop(rt);
        identity.boot_id = Uuid::new_v4();
        let mut rt = Runtime::new(
            Journal::open(self.temp.path(), identity, limits()).unwrap(),
            FakeClock::new(300),
        );
        let replay = rt.next_source_upload(id).unwrap().unwrap();
        match &replay {
            UploadAction::Chunk { request, .. } => assert_eq!(request, &pending),
            _ => panic!("pending chunk expected"),
        }
        accept_upload_action(&mut rt, id, &replay);
        rt
    }

    fn replay_finalize(&self, mut rt: Runtime<FakeClock>) -> (Runtime<FakeClock>, UploadAction) {
        let id = self.id;
        let finalize = loop {
            let next = rt.next_source_upload(id).unwrap().unwrap();
            if matches!(next, UploadAction::Finalize(_)) {
                break next;
            }
            accept_upload_action(&mut rt, id, &next);
        };
        let finalize_id = match &finalize {
            UploadAction::Finalize(r) => r.operation_id,
            _ => unreachable!(),
        };
        drop(rt);
        let mut rt = runtime(self.temp.path());
        let replay = rt.next_source_upload(id).unwrap().unwrap();
        match &replay {
            UploadAction::Finalize(r) => assert_eq!(r.operation_id, finalize_id),
            _ => panic!("same finalize expected"),
        }
        (rt, replay)
    }

    fn acknowledge_receipt(
        &self,
        rt: &mut Runtime<FakeClock>,
        replay: &UploadAction,
        transfer: TransferManifest,
    ) -> CentralRetentionReceipt {
        let id = self.id;
        let mut status = upload_status(rt, id);
        assert_eq!(status.manifest_offset, transfer.manifest_bytes);
        assert_eq!(status.bundle_offset, transfer.bundle_bytes);
        assert!(
            rt.journal.state().attempts[&id]
                .source
                .as_ref()
                .unwrap()
                .receipt
                .is_none()
        );
        assert!(rt.retire_source(id, &self.peer.client).is_err());
        let native: aq_worker::source::GitManifest = serde_json::from_value(
            rt.journal.state().attempts[&id]
                .source
                .as_ref()
                .unwrap()
                .exported
                .as_ref()
                .unwrap()["manifest"]
                .clone(),
        )
        .unwrap();
        let mut receipt = CentralRetentionReceipt {
            receipt_id: Uuid::new_v4(),
            transfer_id: transfer.transfer_id,
            repository_id: transfer.source.repository_id,
            binding_generation: transfer.source.binding_generation,
            source: transfer.checkpoint.clone(),
            retention_handle: transfer.artifact_id,
            manifest_digest: native.canonical_digest().unwrap(),
        };
        receipt.source.hex = "c".repeat(40);
        status.receipt = Some(receipt.clone());
        assert!(
            rt.acknowledge_source_upload(id, replay, status.clone())
                .is_err()
        );
        receipt.source = transfer.checkpoint;
        status.receipt = Some(receipt.clone());
        rt.acknowledge_source_upload(id, replay, status).unwrap();
        assert!(rt.next_source_upload(id).unwrap().is_none());
        receipt
    }

    fn replay_report(&self, mut rt: Runtime<FakeClock>, receipt: CentralRetentionReceipt) {
        let id = self.id;
        let report = rt.journal.state().attempts[&id].report.as_ref().unwrap();
        assert!(report.claimed_success);
        assert_eq!(
            report.artifacts[0].central_receipt,
            Some(receipt.receipt_id.to_string())
        );
        let local_id = report.report_id;
        assert!(report.supersedes_report_id.is_some());
        // Central source retention alone does not authorize retirement.
        assert!(rt.retire_source(id, &self.peer.client).is_err());
        // First network delivery cannot name an unsubmitted local revision as a
        // remote predecessor. Freeze a fresh immutable report before any HTTP.
        rt.journal
            .inject_write_fault(aq_worker::journal::WriteFault::AfterFileSync);
        assert!(rt.prepare_source_report(id).is_err());
        drop(rt);
        let mut rt = runtime(self.temp.path());
        assert_eq!(
            rt.journal.state().attempts[&id]
                .report
                .as_ref()
                .unwrap()
                .report_id,
            local_id
        );
        let sealed = rt.prepare_source_report(id).unwrap();
        assert_ne!(sealed.report_id, local_id);
        assert!(sealed.supersedes_report_id.is_none());
        let bytes = serde_json::to_vec(&sealed).unwrap();
        assert!(rt.export_source(id, &self.peer.client).is_err());
        rt.acknowledge_source(id, receipt).unwrap();
        drop(rt);
        let mut rt = runtime(self.temp.path());
        // A lost response must repeat identical first-delivery bytes and identity.
        assert_eq!(
            serde_json::to_vec(&rt.prepare_source_report(id).unwrap()).unwrap(),
            bytes
        );
        rt.acknowledge_result(ResultResponse {
            report_id: sealed.report_id,
            disposition: ResultDisposition::Accepted,
            retention_release: false,
        })
        .unwrap();
    }
}
#[test]
fn upload_local_commit_failure_keeps_same_pending_chunk_on_reopen() {
    let (_peer, temp, mut rt, id) = upload_fixture();
    let begin = rt.next_source_upload(id).unwrap().unwrap();
    accept_upload_action(&mut rt, id, &begin);
    let action = rt.next_source_upload(id).unwrap().unwrap();
    let request = match &action {
        aq_worker::source_transfer::UploadAction::Chunk { request, .. } => request.clone(),
        _ => unreachable!(),
    };
    let mut status = upload_status(&rt, id);
    status.manifest_offset += u64::from(request.length);
    rt.journal
        .inject_write_fault(aq_worker::journal::WriteFault::AfterFileSync);
    assert!(rt.acknowledge_source_upload(id, &action, status).is_err());
    drop(rt);
    let mut rt = runtime(temp.path());
    let replay = rt.next_source_upload(id).unwrap().unwrap();
    match replay {
        aq_worker::source_transfer::UploadAction::Chunk { request: again, .. } => {
            assert_eq!(request, again)
        }
        _ => panic!("durable pending chunk lost"),
    }
    assert!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .receipt
            .is_none()
    );
}

#[test]
fn upload_changed_retained_bytes_or_symlink_never_replays_changed_payload() {
    use aq_worker::source_transfer::UploadAction;
    let (peer, _temp, mut rt, id) = upload_fixture();
    let begin = rt.next_source_upload(id).unwrap().unwrap();
    accept_upload_action(&mut rt, id, &begin);
    let manifest = rt.next_source_upload(id).unwrap().unwrap();
    accept_upload_action(&mut rt, id, &manifest);
    let action = rt.next_source_upload(id).unwrap().unwrap();
    let request = match action {
        UploadAction::Chunk { request, .. } => request,
        _ => unreachable!(),
    };
    assert_eq!(request.file, TransferFile::Bundle);
    let artifact = rt.journal.state().attempts[&id]
        .source
        .as_ref()
        .unwrap()
        .transfer
        .as_ref()
        .unwrap()
        .artifact_id;
    let path = peer
        .client
        .root()
        .join("artifacts")
        .join(artifact.to_string())
        .join("objects.bundle");
    std::fs::write(&path, vec![b'r'; 600_000]).unwrap();
    assert!(
        rt.next_source_upload(id)
            .err()
            .unwrap()
            .to_string()
            .contains("changed")
    );
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
    assert!(
        rt.next_source_upload(id)
            .err()
            .unwrap()
            .to_string()
            .contains("unsafe")
    );
    assert_eq!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .upload
            .as_ref()
            .unwrap()
            .pending
            .as_ref()
            .unwrap(),
        &request
    );
}
