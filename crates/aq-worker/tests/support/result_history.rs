use super::{ResultInstallationPins, RuntimeIdentity, history::*};
use aq_agent_result_local as wire;
use aq_session_protocol::{ArtifactRef, Digest, LaunchBinding, SessionIdentity, Text};
use aq_worker_protocol::retained_git::Workspace;
use uuid::Uuid;

pub(super) fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub(super) fn binding() -> wire::Binding {
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
pub(crate) fn history() -> ResultHistory {
    ResultHistory {
        schema_version: 1,
        scope: result_scope(),
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
fn pane_exit_keeps_the_exact_unpolled_claim_after_intake_closes() {
    use super::super::stopped::stopped_submission;
    let mut history = history();
    let (_dir, original) = super::pending_submission(&mut history);
    assert!(stopped_submission(&history).is_err());
    history.intake_closed = true;

    let spool = history.scope.root.join(wire::SUBMISSIONS_DIRECTORY);
    let writer = std::fs::File::open(spool.join(wire::WRITER_LOCK_FILE)).unwrap();
    fs2::FileExt::try_lock_exclusive(&writer).unwrap();
    assert!(stopped_submission(&history).is_err());
    drop(writer);

    let claim = stopped_submission(&history).unwrap().unwrap();
    assert_eq!(claim, original);
    assert_eq!(
        wire::encode(&claim).unwrap(),
        std::fs::read(spool.join(wire::SUBMISSION_FILE)).unwrap()
    );
    assert!(history.claim.is_none());
}

fn result_source_binding() -> aq_worker_protocol::SourceBinding {
    use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
    aq_worker_protocol::SourceBinding {
        repository_id: Uuid::from_u128(20),
        binding_generation: 1,
        logical_identity: "fixture".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    }
}
fn result_scope() -> ScopeRecord {
    let b = binding();
    ScopeRecord {
        git_author: None,
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
        runtime: RuntimeIdentity {
            runtime: reference("runtime"),
            qualification: reference("qualification"),
        },
        kernel_boot_id: Uuid::from_u128(30),
        original_deadline_ns: 100,
        original_hard_deadline_ns: 200,
        created_ns: 10,
        recorded_revision: 2,
        source: result_source_binding(),
        workspace: Workspace {
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
    }
}
