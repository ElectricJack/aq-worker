use super::confinement::*;
use aq_session_host::users::AgentIdentity;
use aq_session_protocol::LaunchManifest;

fn fixtures() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../../aq-session-protocol/artifacts/fixtures.json"
    ))
    .unwrap()
}
fn native_manifest() -> LaunchManifest {
    serde_json::from_value(fixtures()["valid_documents"][0]["body"]["manifest"].clone()).unwrap()
}
fn container_manifest() -> LaunchManifest {
    serde_json::from_value(fixtures()["container_manifest"].clone()).unwrap()
}
fn separated() -> AttemptSeparation {
    AttemptSeparation {
        runner_uid: 1001,
        agent: AgentIdentity {
            uid: 1002,
            gid: 1002,
        },
        credentials_attempt_scoped: true,
    }
}
const PERMIT: ExecutionConfinement = ExecutionConfinement {
    permit_container: true,
    require_native: false,
};
fn qualified_vm() -> QualificationRecord {
    QualificationRecord::from_native_probe(
        "aq-rust",
        "boat",
        BackendBoundary::Container,
        NativeProbe {
            outside_write_refused: true,
        },
        "fixture probe",
    )
}

#[test]
fn declared_mode_is_read_from_the_frozen_manifest() {
    assert_eq!(
        ConfinementMode::declared(&native_manifest()).unwrap(),
        ConfinementMode::Native
    );
    assert_eq!(
        ConfinementMode::declared(&container_manifest()).unwrap(),
        ConfinementMode::Container
    );
    let mut both = container_manifest();
    both.harness
        .required_capabilities
        .extend(native_manifest().harness.required_capabilities);
    assert!(ConfinementMode::declared(&both).is_err());
}

#[test]
fn container_mode_requires_profile_permission() {
    let mode = ConfinementMode::declared(&container_manifest()).unwrap();
    let docker = QualificationRecord::docker_local("aq-rust");
    // Admitted only when every SBX19 condition holds.
    let ok = authorize(mode, &PERMIT, &docker, &separated()).unwrap();
    assert_eq!(ok.mode(), ConfinementMode::Container);
    assert_eq!((ok.template(), ok.backend()), ("aq-rust", "docker-local"));
    // 1. The execution profile must explicitly permit container.
    let denied = ExecutionConfinement {
        permit_container: false,
        require_native: false,
    };
    let err = authorize(mode, &denied, &docker, &separated()).unwrap_err();
    assert!(err.to_string().contains("profile"), "{err}");
    let native_only = ExecutionConfinement {
        permit_container: true,
        require_native: true,
    };
    assert!(authorize(mode, &native_only, &docker, &separated()).is_err());
    // 2. The backend must provide the boundary.
    let unbounded = QualificationRecord::from_native_probe(
        "aq-rust",
        "host-shell",
        BackendBoundary::None,
        NativeProbe {
            outside_write_refused: true,
        },
        "fixture probe",
    );
    assert!(authorize(mode, &PERMIT, &unbounded, &separated()).is_err());
    // 3. Runner/agent separation (SBX7): distinct non-root agent.
    for (runner_uid, uid) in [(1001, 0), (1001, 1001)] {
        let mut s = separated();
        s.runner_uid = runner_uid;
        s.agent.uid = uid;
        assert!(authorize(mode, &PERMIT, &docker, &s).is_err());
    }
    let mut root_group = separated();
    root_group.agent.gid = 0;
    assert!(authorize(mode, &PERMIT, &docker, &root_group).is_err());
    // 4. The attempt must hold only its own credentials.
    let mut shared = separated();
    shared.credentials_attempt_scoped = false;
    assert!(authorize(mode, &PERMIT, &docker, &shared).is_err());
}

#[test]
fn native_required_profile_refused_on_container_only_backend() {
    let docker = QualificationRecord::docker_local("aq-rust");
    assert_eq!(docker.default_mode, ConfinementMode::Container);
    assert!(!docker.native_supported);
    let native_only = ExecutionConfinement {
        permit_container: false,
        require_native: true,
    };
    assert!(admit_profile(&native_only, &docker).is_err());
    assert!(admit_profile(&PERMIT, &docker).is_ok());
    // A declared native attempt is refused, never downgraded to container (SBX13),
    // even when the profile would permit container.
    let native = ConfinementMode::declared(&native_manifest()).unwrap();
    assert!(authorize(native, &PERMIT, &docker, &separated()).is_err());
    // A backend whose probe refused outside-workspace writes qualifies native.
    let vm = qualified_vm();
    assert_eq!(vm.default_mode, ConfinementMode::Native);
    assert!(admit_profile(&native_only, &vm).is_ok());
    assert_eq!(
        authorize(native, &native_only, &vm, &separated())
            .unwrap()
            .mode(),
        ConfinementMode::Native
    );
    // A failed probe does not qualify native.
    let failed = QualificationRecord::from_native_probe(
        "aq-rust",
        "boat",
        BackendBoundary::Container,
        NativeProbe {
            outside_write_refused: false,
        },
        "fixture probe",
    );
    assert!(!failed.native_supported);
    assert!(authorize(native, &PERMIT, &failed, &separated()).is_err());
}
