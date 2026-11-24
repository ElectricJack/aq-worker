use super::*;
fn session() -> SessionIdentity {
    SessionIdentity::Central {
        authority_id: Uuid::from_u128(1),
        session_id: Uuid::from_u128(2),
        generation: 1,
    }
}
#[test]
fn independent_permit_is_exact_bounded_and_expiring() {
    let s = session();
    let digest = Digest::of(b"manifest");
    let p = ExecutionPermit {
        session: s.clone(),
        manifest_digest: digest.clone(),
        sequence: 1,
        issued_boot_ms: 100,
        expires_boot_ms: 200,
    };
    assert!(p.validate_at(&s, &digest, 100).is_ok());
    assert!(p.validate_at(&s, &digest, 200).is_err());
    assert!(p.validate_at(&s, &digest, 99).is_err());
    assert!(p.validate_at(&s, &Digest::of(b"changed"), 100).is_err());
    let other = SessionIdentity::Central {
        authority_id: Uuid::from_u128(3),
        session_id: Uuid::from_u128(2),
        generation: 1,
    };
    assert!(p.validate_at(&other, &digest, 100).is_err());
    let mut excessive = p;
    excessive.expires_boot_ms = 100 + MAX_PERMIT_MS + 1;
    assert!(excessive.validate_at(&s, &digest, 100).is_err());
}
#[test]
fn interactive_permit_has_no_window_cap_and_keeps_default_limit_and_expiry() {
    let session = session();
    let digest = Digest::of(b"manifest");
    let mut permit = ExecutionPermit {
        session: session.clone(),
        manifest_digest: digest.clone(),
        sequence: 1,
        issued_boot_ms: 100,
        expires_boot_ms: 100 + MAX_PERMIT_MS,
    };
    assert!(permit.validate_at(&session, &digest, 100).is_ok());
    permit.expires_boot_ms += 1;
    assert!(permit.validate_at(&session, &digest, 100).is_err());
    permit.expires_boot_ms = 100 + 14_400_000;
    assert!(permit.validate_at(&session, &digest, 100).is_err());
    assert!(
        permit
            .validate_at_scoped(&session, &digest, 100, true)
            .is_ok()
    );
    assert!(
        permit
            .validate_at_scoped(&session, &digest, permit.expires_boot_ms, true)
            .is_err()
    );
    assert!(
        permit
            .validate_at_scoped(&session, &Digest::of(b"other"), 100, true)
            .is_err()
    );
    permit.expires_boot_ms = permit.issued_boot_ms;
    assert!(
        permit
            .validate_at_scoped(&session, &digest, 100, true)
            .is_err(),
        "an empty window is still refused"
    );
}
#[test]
fn local_command_bounds_and_protected_environment() {
    let mut c = LocalCommand {
        executable: "/bin/true".into(),
        arguments: vec!["literal; $(text)".into()],
        environment: Default::default(),
    };
    c.validate().unwrap();
    c.environment
        .insert("LD_PRELOAD".into(), "untrusted".into());
    assert!(c.validate().is_err());
    c.environment.clear();
    c.arguments = vec!["x".repeat(4097)];
    assert!(c.validate().is_err());
}
