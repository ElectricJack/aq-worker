use aq_worker::native::adapter::codex::CodexExecInput;
#[test]
fn native_mapping_reuses_preflight_image_after_mutable_program_path_changes() {
    use aq_session_host::observed::NativeExecPool;
    let (root, home, work) = dirs();
    let (offer, payload) = fixture();
    let m = &offer.offered.offer.manifest;
    let program = root.path().join("native");
    std::fs::copy(std::fs::canonicalize("/usr/bin/true").unwrap(), &program).unwrap();
    let image = std::fs::read(&program).unwrap();
    let pin = FilePin {
        path: program.clone(),
        artifact: ArtifactRef {
            id: Text::new("offline.true").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&image),
        },
    };
    let local = helper_pin(root.path());
    let pool = NativeExecPool::new(1024 * 1024, 1).unwrap();
    let mut installed = install_image(&offer, (pin, local), image.len() as u64, &pool);
    std::fs::write(
        &program,
        b"mutable path changed; sealed image remains exact",
    )
    .unwrap();
    let manifest_digest = canonical_digest(m).unwrap();
    let prepared = profile(&offer)
        .prepare_native(
            &mut installed,
            CodexExecInput {
                offer: &offer,
                expected_manifest: &manifest_digest,
                payload: payload.clone(),
                workspace: &work,
                private_home: &home,
            },
        )
        .unwrap();
    let mut received = Vec::new();
    prepared.write_startup(&mut received).unwrap();
    assert_eq!(received, payload);
    assert_eq!(installed.image().unwrap().bytes(), image.len() as u64);
    assert_eq!(pool.charged_bytes().unwrap(), image.len() as u64);
}

fn helper_pin(root: &Path) -> FilePin {
    let helper = root.join("helper");
    std::fs::write(&helper, b"unexecuted helper fixture").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o500)).unwrap();
    FilePin {
        path: helper,
        artifact: ArtifactRef {
            id: Text::new("offline.helper").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"unexecuted helper fixture"),
        },
    }
}

fn install_image(
    offer: &NativeOffer,
    files: (FilePin, FilePin),
    image_bytes: u64,
    pool: &aq_session_host::observed::NativeExecPool,
) -> aq_worker::native::adapter::native_exec::NativeExecInstallation {
    use aq_worker::native::adapter::native_exec::{
        NativeExecInstallation, NativeExecKind, NativeExecManifest,
    };
    let (pin, local) = files;
    let m = &offer.offered.offer.manifest;
    let r = m.harness.adapter.clone();
    let renderer = offer.startup.renderer_qualification.clone();
    let manifest = NativeExecManifest {
        schema_version: 1,
        profile: NativeExecKind::Codex0166ExecStdinV1,
        executable_bytes: image_bytes,
        installation: AdapterManifest {
            schema_version: 1,
            adapter: r.clone(),
            runtime: r.clone(),
            qualification: r.clone(),
            renderer: renderer.clone(),
            handoff: HandoffKind::OpaquePrivateFileV1,
            executable: pin,
            helper: local.clone(),
            tmux: local,
        },
    };
    let digest = manifest.digest().unwrap();
    NativeExecInstallation::open(
        manifest,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &r,
            runtime: &r,
            qualification: &r,
            renderer: &renderer,
        },
        pool,
    )
    .unwrap()
}
