#![allow(dead_code)]
use aq_admission::*;
use uuid::Uuid;
pub fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
pub fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
pub fn binding() -> LaunchBinding {
    LaunchBinding {
        schema_version: 1,
        session: SessionIdentity::Remote {
            node_id: id(11),
            boot_id: id(12),
            attempt_id: id(13),
            assignment_generation: 1,
            session_id: id(14),
        },
        offer_id: id(15),
        attempt_id: id(13),
        assignment_generation: 1,
        execution_spec_digest: Digest::of(b"spec"),
        launch_manifest_digest: Digest::of(b"manifest"),
        graph_access_profile_digest: Digest::of(b"graph-access"),
    }
}
pub fn lease() -> MaterialLease {
    MaterialLease {
        schema_version: 1,
        lease_id: id(16),
        binding: binding(),
        credential_profile: artifact("credential-profile"),
        credential_revision: 1,
        provider: Label::new("fixture-provider").unwrap(),
        account_reference: Label::new("fixture-account").unwrap(),
        endpoint_reference: Label::new("fixture-endpoint").unwrap(),
        purpose: Label::new("model-authentication").unwrap(),
        material_digest: Digest::of(b"synthetic-key"),
        material_bytes: 13,
        issued_at_ms: 1000,
        expires_at_ms: 2000,
    }
}
pub fn source() -> SourceAdmission {
    SourceAdmission {
        schema_version: 1,
        project_id: id(31),
        project_revision: 1,
        repository_id: id(41),
        repository_binding_generation: 1,
        policy: artifact("source-policy"),
        selection: SourceSelection::NamedRef {
            reference: Label::new("refs/heads/main").unwrap(),
        },
        resolved_object: ObjectId {
            format: ObjectFormat::Sha1,
            hex: "a".repeat(40),
        },
        observed_at_ms: 1000,
        expires_at_ms: 2000,
    }
}
pub fn retained() -> RetainedArtifact {
    RetainedArtifact {
        artifact: ArtifactRef {
            digest: Digest::of(b"actual-artifact"),
            ..artifact("context")
        },
        kind: ArtifactKind::Context,
        bytes: 15,
        retention_receipt: artifact("receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    }
}
pub fn candidate(project: u128, task: u128) -> PlacementCandidate {
    PlacementCandidate {
        project_id: id(project),
        task: TaskRevision {
            workspace_id: id(21),
            task_id: id(task),
            version: 1,
            content_revision: 1,
        },
        selection: artifact("sealed-selection"),
        priority: 0,
        queued_at_ms: 1000,
        expires_at_ms: 2000,
        resources: Resources {
            slots: 1,
            memory_bytes: 100,
            cpu_millis: 100,
            disk_bytes: 100,
        },
        nodes: vec![NodeEligibility {
            node_id: id(11),
            boot_id: id(12),
            eligible: true,
            source_warm: false,
            reason_codes: vec![],
        }],
    }
}
pub fn placement() -> PlacementInput {
    PlacementInput {
        schema_version: 1,
        now_ms: 1500,
        after_project: None,
        max_offers: 2,
        candidates: vec![candidate(31, 51), candidate(31, 52), candidate(32, 53)],
        nodes: vec![NodeCapacity {
            node_id: id(11),
            boot_id: id(12),
            revision: 1,
            observed_at_ms: 1000,
            expires_at_ms: 2000,
            available: Resources {
                slots: 2,
                memory_bytes: 200,
                cpu_millis: 200,
                disk_bytes: 200,
            },
        }],
    }
}
pub fn documents() -> Vec<ContractDocument> {
    let lease = lease();
    vec![
        ContractDocument::SourceAdmission(source()),
        ContractDocument::RetainedArtifact(retained()),
        ContractDocument::MaterialLease(lease.clone()),
        ContractDocument::MaterialReceipt(MaterialReceipt {
            schema_version: 1,
            operation_id: id(17),
            lease_id: lease.lease_id,
            binding: lease.binding.clone(),
            material_digest: lease.material_digest.clone(),
            private_binding: artifact("worker-private-credential-binding"),
            received_at_ms: 1500,
        }),
        ContractDocument::PlacementInput(placement()),
        ContractDocument::PlacementOutput(place(&placement()).unwrap()),
    ]
}
