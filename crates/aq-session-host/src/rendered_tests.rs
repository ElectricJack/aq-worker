use super::*;
fn fixture(bytes: &[u8]) -> (LaunchManifest, ArtifactRef, ArtifactRef) {
    let mut manifest: LaunchManifest =
        serde_json::from_str(include_str!("../tests/fixtures/manifest.json")).unwrap();
    let digest = Digest::of(b"synthetic descriptor; full descriptor validation belongs to runtime");
    let descriptor = ArtifactRef {
        id: aq_session_protocol::Text::new(format!("aq.startup-delivery/{}", digest.as_str()))
            .unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest,
    };
    manifest.modules.push(descriptor.clone());
    let payload = ArtifactRef {
        id: aq_session_protocol::Text::new("final-startup").unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    };
    (manifest, descriptor, payload)
}
#[test]
fn final_payload_is_literal_and_bound_to_exact_single_descriptor() {
    let bytes = "framing\nfixture\n雪\0literal".as_bytes();
    let (m, d, p) = fixture(bytes);
    let prepared = RenderedStartup::new(m.clone(), d.clone(), p.clone(), bytes.to_vec()).unwrap();
    assert_eq!(prepared.bytes, bytes);
    let mut missing = m.clone();
    missing.modules.retain(|r| r != &d);
    assert!(RenderedStartup::new(missing, d.clone(), p.clone(), bytes.to_vec()).is_err());
    let mut multiple = m.clone();
    let mut other = d.clone();
    other.digest = Digest::of(b"other");
    other.id =
        aq_session_protocol::Text::new(format!("aq.startup-delivery/{}", other.digest.as_str()))
            .unwrap();
    multiple.modules.push(other);
    assert!(RenderedStartup::new(multiple, d.clone(), p.clone(), bytes.to_vec()).is_err());
    assert!(RenderedStartup::new(m.clone(), d.clone(), p.clone(), b"changed".to_vec()).is_err());
    let mut bounded = m.clone();
    bounded.context.startup_byte_budget = 7;
    assert!(RenderedStartup::new(bounded, d.clone(), p.clone(), bytes.to_vec()).is_err());
    let (m, d, p) = fixture(&[0xff]);
    assert!(RenderedStartup::new(m, d, p, vec![0xff]).is_err());
    let (m, d, p) = fixture(&[]);
    assert!(RenderedStartup::new(m, d, p, vec![]).is_err());
}
#[test]
fn legacy_sealed_configuration_omits_new_optional_field() {
    let (manifest, _, _) = fixture(b"payload");
    let permit = ExecutionPermit {
        session: manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: 1,
        expires_boot_ms: 100,
    };
    let sealed = SealedLaunch {
        agent_identity: None,
        launch_id: Uuid::from_u128(1),
        manifest: manifest.into(),
        permit,
        workspace: "/tmp/work".into(),
        helper: "/tmp/helper".into(),
        tmux: "/usr/bin/tmux".into(),
        tmux_version: "tmux 3.4".into(),
        command: LocalCommand {
            executable: "/bin/true".into(),
            arguments: vec![],
            environment: Default::default(),
        },
        resource_policy: ResourcePolicy::CallerAdmission(ArtifactRef {
            id: aq_session_protocol::Text::new("fixture").unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(b"fixture"),
        }),
        resource_qualification: None,
        rendered_startup: None,
        observed: None,
        private_input: None,
    };
    assert_eq!(
        serde_json::to_vec(&sealed.manifest).unwrap(),
        serde_json::to_vec(sealed.manifest.task().unwrap()).unwrap()
    );
    assert_eq!(
        canonical_digest(&sealed.manifest).unwrap(),
        canonical_digest(sealed.manifest.task().unwrap()).unwrap()
    );
    let bytes = serde_json::to_vec(&sealed).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("rendered_startup").is_none());
    assert!(value.get("observed").is_none());
    let read: SealedLaunch = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_vec(&read).unwrap(), bytes);
    assert_eq!(read.intent().unwrap(), sealed.intent().unwrap());
}
