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
pub(super) fn history() -> ResultHistory {
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
            artifact_limit_bytes: 10000,
            operations: SourceOperations {
                checkpoint: Uuid::from_u128(40),
                export: Uuid::from_u128(41),
                transfer: Uuid::from_u128(42),
                upload_begin: Uuid::from_u128(43),
                upload_finalize: Uuid::from_u128(44),
            },
        },
        intake_closed: false,
        claim: None,
        stop: None,
        source: SourceHistory::default(),
        report: None,
        receipt: None,
        failure: None,
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
    h.scope.operations.export = h.scope.operations.checkpoint;
    assert!(h.validate().is_err());
    h.scope.operations.export = Uuid::from_u128(41);
    h.scope.original_deadline_ns = 201;
    assert!(h.validate().is_err());
    h.scope.original_deadline_ns = 100;
    h.scope.reserved_disk_bytes = 29999;
    assert!(h.validate().is_err());
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
