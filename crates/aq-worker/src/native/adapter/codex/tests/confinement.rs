fn authorized(mode: ConfinementMode) -> AuthorizedConfinement {
    use crate::native::runner::sandbox::confinement::*;
    let separation = AttemptSeparation {
        runner_uid: 1001,
        agent: aq_session_host::users::AgentIdentity {
            uid: 1002,
            gid: 1002,
        },
        credentials_attempt_scoped: true,
    };
    let permit = ExecutionConfinement {
        permit_container: true,
        require_native: false,
    };
    let record = QualificationRecord::from_native_probe(
        "aq-rust",
        "fixture",
        BackendBoundary::Container,
        NativeProbe {
            outside_write_refused: true,
        },
        "fixture probe",
    );
    authorize(mode, &permit, &record, &separation).unwrap()
}
fn with_confinement_caps(offer: &NativeOffer, ids: &[&str]) -> (NativeOffer, Digest) {
    let mut offer = offer.clone();
    let profiled = &mut offer.offered.offer;
    for caps in [
        &mut profiled.manifest.harness.required_capabilities,
        &mut profiled.required_capabilities,
    ] {
        caps.retain(|c| {
            c.id.as_str() != aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY
                && c.id.as_str() != aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY
        });
        for id in ids {
            caps.push(aq_session_protocol::Capability {
                id: Text::new(*id).unwrap(),
                version: 1,
            });
        }
    }
    let m = &offer.offered.offer.manifest;
    let digest = crate::native::journal::canonical_digest(m).unwrap();
    offer.offered.offer.binding.launch_manifest_digest = digest.clone();
    (offer, digest)
}
#[test]
fn container_manifest_maps_only_with_authorized_container_confinement() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (native, payload) = fixture();
    let (offer, digest) = with_confinement_caps(
        &native,
        &[aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY],
    );
    offer.validate().unwrap();
    let mut adapter = installed(root.path(), &offer);
    let prepare = |selected: &CodexExecProfile, adapter: &mut InstalledAdapter| {
        selected.prepare(adapter, &offer, &digest, payload.clone(), &work, &home)
    };
    // No proof, or a native proof: refused, never silently native (SBX13).
    let mut selected = profile(&offer);
    let err = prepare(&selected, &mut adapter).err().unwrap();
    assert!(err.to_string().contains("AuthorizedConfinement"), "{err}");
    selected.confinement = Some(authorized(ConfinementMode::Native));
    let err = prepare(&selected, &mut adapter).err().unwrap();
    assert!(err.to_string().contains("differs from authorized"), "{err}");
    // With the sandbox's container proof it maps through map_container.
    selected.confinement = Some(authorized(ConfinementMode::Container));
    let prepared = prepare(&selected, &mut adapter).unwrap();
    let args = &prepared.command().arguments;
    let sandbox = args.iter().position(|a| a == "--sandbox").unwrap();
    assert_eq!(args[sandbox + 1], "danger-full-access");
    assert_eq!(args.last().unwrap(), "-");
    // A native manifest refuses a container proof.
    let native_digest =
        crate::native::journal::canonical_digest(&native.offered.offer.manifest).unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut mismatched = profile(&native);
    mismatched.confinement = Some(authorized(ConfinementMode::Container));
    let err = mismatched
        .prepare(
            &mut installed(other.path(), &native),
            &native,
            &native_digest,
            payload.clone(),
            &work,
            &home,
        )
        .err()
        .unwrap();
    assert!(err.to_string().contains("differs from authorized"), "{err}");
}
#[test]
fn harness_declaring_both_confinement_modes_is_refused() {
    let (native, _) = fixture();
    let (both, _) = with_confinement_caps(
        &native,
        &[
            aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY,
            aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY,
        ],
    );
    assert!(both.validate().is_err());
    let harness = &both.offered.offer.manifest.harness;
    for proof in [
        None,
        Some(authorized(ConfinementMode::Native)),
        Some(authorized(ConfinementMode::Container)),
    ] {
        let err = selected_confinement(harness, proof.as_ref(), false).unwrap_err();
        assert!(err.to_string().contains("both"), "{err}");
    }
    let (container, _) = with_confinement_caps(
        &native,
        &[aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY],
    );
    let proof = authorized(ConfinementMode::Container);
    let harness = &container.offered.offer.manifest.harness;
    assert_eq!(
        selected_confinement(harness, Some(&proof), false).unwrap(),
        ConfinementMode::Container
    );
    assert!(selected_confinement(harness, Some(&proof), true).is_err());
}
