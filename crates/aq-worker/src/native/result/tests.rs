use super::*;
use aq_session_protocol::{SessionIdentity, Text};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn binding() -> wire::Binding {
    let session = SessionIdentity::Remote {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
        attempt_id: Uuid::from_u128(3),
        assignment_generation: 4,
        session_id: Uuid::from_u128(5),
    };
    wire::Binding {
        schema_version: 1,
        launch: LaunchBinding {
            schema_version: 1,
            session,
            offer_id: Uuid::from_u128(6),
            attempt_id: Uuid::from_u128(3),
            assignment_generation: 4,
            execution_spec_digest: Digest::of(b"spec"),
            launch_manifest_digest: Digest::of(b"manifest"),
            graph_access_profile_digest: Digest::of(b"graph"),
        },
        execution_record_digest: Digest::of(b"data only; not live authority"),
        expected_outcome: reference("aq.expected-outcome"),
        operation_id: Uuid::from_u128(7),
        report_id: Uuid::from_u128(8),
        nonce: Uuid::from_u128(9),
    }
}
fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .unwrap();
    let channel = temp.path().join("result");
    (temp, workspace, channel)
}
fn publish(storage: &ChannelStorage) -> wire::Binding {
    let b = binding();
    let presence = wire::Presence {
        schema_version: 1,
        binding_digest: b.digest().unwrap(),
        kernel_boot_id: Uuid::from_u128(99),
        original_execution_deadline_boot_ns: 100,
        original_hard_deadline_boot_ns: 200,
    };
    storage.publish(&b, &presence).unwrap();
    storage.verify_control(&b, &presence).unwrap();
    b
}
#[test]
fn result_storage_control_custody_and_first_intake_lock_are_actual() {
    let (_temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let b = publish(&storage);
    assert!(storage.intake_open().unwrap());
    assert!(storage.read_submission().unwrap().is_none());
    let submission = wire::Submission::new(
        b,
        wire::SubmissionInput {
            schema_version: 1,
            conclusion: wire::Conclusion::new("Change made; tests passed; limits unknown").unwrap(),
        },
    )
    .unwrap();
    let bytes = wire::encode(&submission).unwrap();
    let p = storage.submissions().join("submission.json");
    std::fs::write(&p, &bytes).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(storage.read_submission().unwrap().unwrap(), bytes);
    storage.close_intake();
    assert!(!storage.intake_open().unwrap());
    let presence = std::fs::File::open(path.join("owner.lock")).unwrap();
    assert!(
        fs2::FileExt::try_lock_shared(&presence).is_err(),
        "completion owner remains present after closing new intake"
    );
    storage.release_presence();
    fs2::FileExt::try_lock_shared(&presence).unwrap();
    assert!(ChannelStorage::create(&path, &workspace).is_err()); // Never adopt a previous directory.
}
#[test]
fn result_storage_refuses_route_replacement_and_workspace_overlap() {
    let (temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    std::fs::rename(&path, temp.path().join("retained-original")).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    assert!(storage.verify().is_err());
    assert!(ChannelStorage::create(&workspace.join("result"), &workspace).is_err());
}
#[test]
fn result_storage_refuses_links_special_members_and_wrong_modes() {
    let (temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let p = storage.submissions().join("submission.json");
    let foreign = temp.path().join("foreign");
    std::fs::write(&foreign, b"{}").unwrap();
    std::os::unix::fs::symlink(&foreign, &p).unwrap();
    assert!(storage.read_submission().is_err());
    std::fs::remove_file(&p).unwrap();
    std::fs::hard_link(&foreign, &p).unwrap();
    assert!(storage.read_submission().is_err());
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, b"{}").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(storage.read_submission().is_err());
}
#[test]
fn result_ack_is_projection_and_unknown_pending_is_preserved() {
    let (_temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let b = publish(&storage);
    let ack = wire::Acknowledgment {
        schema_version: 1,
        operation_id: b.operation_id,
        report_id: b.report_id,
        binding_digest: b.digest().unwrap(),
        submission_digest: Digest::of(b"example"),
        phase: wire::Phase::Accepted,
    };
    storage.write_ack(&ack).unwrap();
    assert_eq!(
        std::fs::read(path.join("ack.json")).unwrap(),
        wire::encode(&ack).unwrap()
    );
    std::fs::write(path.join("ack.pending"), b"unknown prior write").unwrap();
    assert!(storage.write_ack(&ack).is_err());
    assert_eq!(
        std::fs::read(path.join("ack.pending")).unwrap(),
        b"unknown prior write"
    );
}
pub(crate) fn history() -> ResultHistory {
    use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
    let b = binding();
    let source = aq_worker_protocol::SourceBinding {
        repository_id: Uuid::from_u128(20),
        binding_generation: 1,
        logical_identity: "fixture".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    };
    ResultHistory {
        schema_version: 1,
        scope: ScopeRecord {
            binding: b,
            contract: wire::artifact(),
            installation: ResultInstallationPins {
                adapter: reference("aq.codex-result-local-adapter"),
                contract: wire::artifact(),
                client_manifest: reference("aq.agent-result-client-installation"),
                client_executable: reference("client"),
                environment: reference("env"),
                recipe: aq_codex_mapper::result_local_recipe_artifact(),
                permission_digest: Digest::of(b"permissions"),
                reserved_client_bytes: 64 * 1024 * 1024,
            },
            root: "/result".into(),
            git_pin: DirectoryPin {
                device: 1,
                inode: 7,
            },
            workspace_pin: DirectoryPin {
                device: 1,
                inode: 8,
            },
            storage: StorageStage::Ready {
                pins: StoragePins {
                    root: DirectoryPin {
                        device: 1,
                        inode: 2,
                    },
                    submissions: DirectoryPin {
                        device: 1,
                        inode: 3,
                    },
                },
                recorded_revision: 3,
            },
            runtime: crate::native::journal::RuntimeIdentity {
                runtime: reference("runtime"),
                qualification: reference("qualification"),
            },
            kernel_boot_id: Uuid::from_u128(30),
            original_deadline_ns: 100,
            original_hard_deadline_ns: 200,
            created_ns: 10,
            recorded_revision: 2,
            source,
            workspace: crate::source::Workspace {
                workspace_id: Uuid::from_u128(31),
                generation: 1,
                path: "/git/workspaces/one".into(),
            },
            managed_root: "/git".into(),
            reserved_disk_bytes: 30000,
            workspace_limit_bytes: 10000,
            operations: SourceOperations {
                checkpoint: Uuid::from_u128(40),
                transfer: Uuid::from_u128(42),
                pushed: Uuid::from_u128(43),
            },
        },
        intake_closed: false,
        claim: None,
        stop: None,
        source: SourceHistory::default(),
        report: None,
        receipt: None,
        failure: None,
        salvaged: None,
    }
}
#[test]
fn result_history_exact_claim_identity_order_and_canonical_bytes() {
    let mut h = history();
    h.validate().unwrap();
    let value = wire::Submission::new(
        h.scope.binding.clone(),
        wire::SubmissionInput {
            schema_version: 1,
            conclusion: wire::Conclusion::new("Explicit claim").unwrap(),
        },
    )
    .unwrap();
    let bytes = wire::encode(&value).unwrap();
    h.claim = Some(ClaimRecord {
        bytes: bytes.clone(),
        bytes_digest: Digest::of(&bytes),
        submission_digest: value.digest().unwrap(),
        recorded_revision: 4,
    });
    h.intake_closed = true;
    h.validate().unwrap();
    assert!(matches!(
        h.acknowledgment().unwrap().unwrap().phase,
        wire::Phase::Accepted
    ));
    h.claim.as_mut().unwrap().recorded_revision = 3;
    assert!(h.validate().is_err());
    h.claim.as_mut().unwrap().recorded_revision = 4;
    h.claim.as_mut().unwrap().bytes.push(b' ');
    assert!(h.validate().is_err());
}
#[test]
fn result_history_protects_scope_budget_and_does_not_project_unclaimed_success() {
    let mut h = history();
    assert!(h.acknowledgment().unwrap().is_none());
    h.scope.operations.pushed = h.scope.operations.checkpoint;
    assert!(h.validate().is_err());
    h.scope.operations.pushed = Uuid::from_u128(43);
    h.scope.original_deadline_ns = 201;
    assert!(h.validate().is_err());
    h.scope.original_deadline_ns = 100;
    h.scope.reserved_disk_bytes = 19999;
    assert!(h.validate().is_err());
}
/// A claimed, actually stopped history whose checkpoint aq-git recorded.
fn checkpointed() -> ResultHistory {
    let mut h = history();
    let value = wire::Submission::new(
        h.scope.binding.clone(),
        wire::SubmissionInput {
            schema_version: 1,
            conclusion: wire::Conclusion::new("Explicit claim").unwrap(),
        },
    )
    .unwrap();
    let bytes = wire::encode(&value).unwrap();
    h.claim = Some(ClaimRecord {
        bytes_digest: Digest::of(&bytes),
        bytes,
        submission_digest: value.digest().unwrap(),
        recorded_revision: 4,
    });
    h.intake_closed = true;
    h.stop = Some(StopAnchor {
        proof_digest: Digest::of(b"proof"),
        native_stopped_revision: 5,
        recorded_revision: 6,
        stopped_receipt: reference("stopped"),
        outcome: aq_admission::transport::ProcessOutcome::Failed,
    });
    let head = aq_worker_protocol::ObjectId {
        algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
        hex: "b".repeat(40),
    };
    let checkpoint = crate::source::Checkpoint {
        workspace_id: h.scope.workspace.workspace_id,
        workspace_generation: 2,
        source: aq_worker_protocol::ObjectId {
            hex: "c".repeat(40),
            ..head.clone()
        },
        retention_handle: Uuid::from_u128(50),
    };
    let operation = h.scope.operations.checkpoint;
    let action = crate::source::GitAction::Checkpoint {
        workspace_id: h.scope.workspace.workspace_id,
        generation: h.scope.workspace.generation,
        head: head.clone(),
    };
    h.source.head = Some(head);
    h.source.workspace_bytes = Some(100);
    h.source.operations.insert(
        "checkpoint".into(),
        crate::source::PendingGitOperation {
            operation_id: operation,
            request: action.request(&h.scope.source, operation),
            result: Some(serde_json::to_value(&checkpoint).unwrap()),
        },
    );
    h.source.checkpoint = Some(checkpoint);
    h.validate().unwrap();
    h
}
fn receipt(h: &ResultHistory) -> aq_worker_protocol::CentralRetentionReceipt {
    aq_worker_protocol::CentralRetentionReceipt {
        receipt_id: Uuid::from_u128(60),
        transfer_id: h.scope.operations.transfer,
        repository_id: h.scope.source.repository_id,
        binding_generation: h.scope.source.binding_generation,
        source: h.source.checkpoint.as_ref().unwrap().source.clone(),
        retention_handle: Uuid::from_u128(61),
        manifest_digest: "d".repeat(64),
    }
}
#[test]
fn result_push_intent_needs_a_stopped_checkpoint_and_replays_the_original_request() {
    // Neither an unstopped nor an unclaimed owner can hold a checkpoint or a push.
    for strip in [0, 1] {
        let mut early = checkpointed();
        if strip == 0 {
            early.stop = None;
        } else {
            early.claim = None;
        }
        assert!(early.validate().is_err());
    }
    let mut bare = history();
    bare.begin_push().unwrap();
    assert!(bare.validate().is_err(), "push intent before a checkpoint");
    assert!(bare.pushed_request().is_err());

    let mut h = checkpointed();
    assert!(h.pushed_request().is_err(), "no request before its intent");
    h.begin_push().unwrap();
    h.validate().unwrap();
    assert!(!h.source.push.as_ref().unwrap().pushed);
    let request = serde_json::to_value(h.pushed_request().unwrap()).unwrap();
    assert_eq!(
        request,
        serde_json::json!({
            "identity": {"node_id": Uuid::from_u128(1), "boot_id": Uuid::from_u128(2)},
            "operation_id": Uuid::from_u128(43),
            "transfer_id": Uuid::from_u128(42),
            "attempt": {"attempt_id": Uuid::from_u128(3), "session_id": Uuid::from_u128(5), "generation": 4},
            "resolved_spec_digest": Digest::of(b"spec").as_str(),
            "source": h.scope.source,
            "checkpoint": {"algorithm": "sha1", "hex": "c".repeat(40)},
        })
    );
    // A reopened journal replays the same IDs and checkpoint, never a second intent.
    let mut reopened: ResultHistory =
        serde_json::from_slice(&serde_json::to_vec(&h).unwrap()).unwrap();
    reopened.validate().unwrap();
    assert_eq!(reopened.digest().unwrap(), h.digest().unwrap());
    assert_eq!(
        serde_json::to_value(reopened.pushed_request().unwrap()).unwrap(),
        request
    );
    assert!(reopened.begin_push().is_err());
    reopened.source.push.as_mut().unwrap().pushed = true;
    assert_eq!(
        serde_json::to_value(reopened.pushed_request().unwrap()).unwrap(),
        request
    );
    h.source.push.as_mut().unwrap().identity.boot_id = Uuid::from_u128(77);
    assert!(h.validate().is_err(), "another boot cannot own the push");
}
#[test]
fn result_receipt_needs_the_completed_push_of_this_checkpoint() {
    let mut h = checkpointed();
    h.begin_push().unwrap();
    h.source.receipt = Some(receipt(&h));
    assert!(h.validate().is_err(), "receipt before the push completed");
    h.source.push.as_mut().unwrap().pushed = true;
    h.validate().unwrap();
    let good = receipt(&h);
    let other = aq_worker_protocol::ObjectId {
        hex: "e".repeat(40),
        ..good.source.clone()
    };
    let changes: [fn(&mut aq_worker_protocol::CentralRetentionReceipt); 5] = [
        |r| r.transfer_id = Uuid::from_u128(70),
        |r| r.repository_id = Uuid::from_u128(71),
        |r| r.binding_generation += 1,
        |r| r.retention_handle = Uuid::nil(),
        |r| r.receipt_id = Uuid::nil(),
    ];
    for change in changes {
        let mut wrong = good.clone();
        change(&mut wrong);
        h.source.receipt = Some(wrong);
        assert!(h.validate().is_err());
    }
    h.source.receipt = Some(aq_worker_protocol::CentralRetentionReceipt {
        source: other,
        ..good
    });
    assert!(h.validate().is_err(), "receipt for another commit");
}
#[test]
fn result_history_of_the_bundle_upload_format_is_not_read() {
    let now = serde_json::to_value(checkpointed()).unwrap();
    let mut old = now.clone();
    old["scope"]["operations"]["export"] = serde_json::json!(Uuid::from_u128(41));
    assert!(serde_json::from_value::<ResultHistory>(old).is_err());
    let mut old = now.clone();
    old["source"]["export"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<ResultHistory>(old).is_err());
    let mut old = now.clone();
    old["scope"]["operations"]
        .as_object_mut()
        .unwrap()
        .remove("pushed");
    assert!(serde_json::from_value::<ResultHistory>(old).is_err());
    serde_json::from_value::<ResultHistory>(now).unwrap();
}
#[test]
fn result_module_alone_does_not_prove_counted_startup_delivery() {
    let value:serde_json::Value=serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-v2.fixture.json")).unwrap();
    let mut offer: aq_admission::startup::NativeOffer =
        serde_json::from_value(value["proposal"]["original"].clone()).unwrap();
    let startup = wire::startup_artifact();
    offer.offered.offer.manifest.modules.push(startup.clone());
    assert!(!counted_startup(&offer));
    let mut context = offer.offered.offer.manifest.context.references[0].clone();
    context.artifact = startup.clone();
    context.bytes = wire::STARTUP_TEXT.len() as u64;
    context.required = true;
    context.delivery = aq_session_protocol::ContextDelivery::Startup;
    offer
        .offered
        .offer
        .manifest
        .context
        .references
        .push(context);
    assert!(!counted_startup(&offer));
    offer
        .startup
        .startup_inputs
        .push(aq_admission::startup::InputSpan {
            artifact: startup,
            offset: 0,
            bytes: wire::STARTUP_TEXT.len() as u64,
        });
    assert!(counted_startup(&offer));
}
#[test]
fn result_format11_header_is_refused_before_pending_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("journal");
    let limits = crate::native::journal::Limits {
        slots: 1,
        memory_bytes: 1000,
        cpu_millis: 1000,
        disk_bytes: 100000,
    };
    let node = Uuid::new_v4();
    let journal = crate::native::journal::Journal::create(&root, node, limits.clone()).unwrap();
    drop(journal);
    let file = root.join("journal.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["state"]["format"] = 11.into();
    let raw = serde_json::to_vec(&value).unwrap();
    std::fs::write(&file, &raw).unwrap();
    let pending = root.join("journal.pending");
    std::fs::write(&pending, b"preserve ambiguous old-format evidence").unwrap();
    assert!(crate::native::journal::Journal::open(&root, node, limits).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), raw);
    assert_eq!(
        std::fs::read(&pending).unwrap(),
        b"preserve ambiguous old-format evidence"
    );
}

#[test]
fn result_namespace_is_owned_and_allocates_disjoint_attempt_children() {
    let (temp, _, _) = directories();
    let path = temp.path().join("results");
    let ns = storage::Namespace::create(&path).unwrap();
    let first = ns.attempt(&path, Uuid::from_u128(1)).unwrap();
    let second = ns.attempt(&path, Uuid::from_u128(2)).unwrap();
    assert_ne!(first, second);
    assert_eq!(first.parent(), Some(path.as_path()));
    assert!(
        storage::Namespace::create(&path).is_err(),
        "a new owner cannot adopt a retained namespace"
    );
    std::fs::rename(&path, temp.path().join("retained-results")).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    assert!(ns.attempt(&path, Uuid::from_u128(3)).is_err());
}
