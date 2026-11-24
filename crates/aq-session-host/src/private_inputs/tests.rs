use super::*;
pub(super) fn login(refresh: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":"offline-id","access_token":"offline-access","refresh_token":refresh,"account_id":"offline-account"},"last_refresh":"2026-11-01T00:00:00Z"})).unwrap()
}
#[test]
fn subscription_login_is_explicit_closed_bounded_and_errors_hide_tokens() {
    PrivateLaunchInput::validate_chatgpt_auth_json(&login("offline-refresh")).unwrap();
    for bytes in [b"sk-api-key".to_vec(), login(""), vec![b'x';65537], br#"{"auth_mode":"chatgpt","auth_mode":"chatgpt","tokens":{"access_token":"private-value"}}"#.to_vec()] {
            let error=PrivateLaunchInput::validate_chatgpt_auth_json(&bytes).unwrap_err();
            assert!(!format!("{error:#}").contains("private-value"));
        }
}
#[test]
fn interactive_executor_private_config_trusts_exactly_launch_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let (root, publication, bytes) = reserve_test_login(dir.path());
    let record = publication.record();
    assert!(
        !serde_json::to_string(&record)
            .unwrap()
            .contains("offline-access")
    );
    let workspace = dir.path().join("launch workspace.\"quoted\"\\name");
    let _held = publication.publish(&workspace).unwrap();
    assert_trusted_configuration(&root, &workspace, &bytes);
    verify_record(&File::open(&root).unwrap(), &record, true).unwrap();
    let replacement = root.join("codex-home/new-auth");
    std::fs::write(&replacement, login("offline-rotated-refresh")).unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::rename(&replacement, root.join("codex-home/auth.json")).unwrap();
    assert!(verify_record(&File::open(&root).unwrap(), &record, true).is_err());
    let refusal = verify_record(&File::open(&root).unwrap(), &record, false).unwrap_err();
    assert!(!format!("{refusal:#}").contains("offline-rotated-refresh"));
    assert_eq!(
        std::fs::read(root.join("codex-home/auth.json")).unwrap(),
        login("offline-rotated-refresh"),
        "rotated private login must remain available for reconciliation"
    );
    assert_eq!(
        std::fs::read(dir.path().join("source/material.bin")).unwrap(),
        bytes,
        "private refresh never overwrites operator material"
    );
}
#[test]
fn subscription_publication_refuses_changed_original_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let (root, publication, _) = reserve_test_login(dir.path());
    std::fs::write(dir.path().join("source/material.bin"), login("changed")).unwrap();
    assert!(publication.publish(Path::new("/launch/workspace")).is_err());
    assert_eq!(
        std::fs::metadata(root.join("codex-home/auth.json"))
            .unwrap()
            .len(),
        0
    );
}
pub(super) fn scoped() -> (ScopedCodexAuthBinding, LaunchManifest) {
    let manifest: LaunchManifest =
        serde_json::from_str(include_str!("../../tests/fixtures/manifest.json")).unwrap();
    let binding = aq_session_protocol::LaunchBinding {
        schema_version: 1,
        session: manifest.bindings.session.clone(),
        offer_id: Uuid::new_v4(),
        attempt_id: manifest.bindings.attempt_id,
        assignment_generation: manifest.bindings.assignment_generation,
        execution_spec_digest: Digest::of(b"offline exact execution"),
        launch_manifest_digest: canonical_digest(&manifest).unwrap(),
        graph_access_profile_digest: manifest.bindings.graph_access_profile.digest.clone(),
    };
    (
        ScopedCodexAuthBinding {
            binding,
            launch_id: Uuid::new_v4(),
            origin: "http://127.0.0.1:12345".into(),
            deadline_boot_ms: 10000,
        },
        manifest,
    )
}
pub(super) fn source(root: &Path) -> (File, Digest, u64, Digest) {
    let dir = root.join("source");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let key = b"sk-private-offline-only";
    let record = b"{\"schema_version\":1}";
    for (name, bytes) in [
        ("material.bin", key.as_slice()),
        ("record.json", record.as_slice()),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (
        File::open(dir).unwrap(),
        Digest::of(key),
        key.len() as u64,
        Digest::of(record),
    )
}
pub(super) fn root(path: &Path) -> PathBuf {
    let p = path.join("launch");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    p
}
#[test]
fn reserved_metadata_has_no_auth_bytes_and_original_record_is_rechecked() {
    let dir = tempfile::tempdir().unwrap();
    let (fd, digest, bytes, record) = source(dir.path());
    let root = root(dir.path());
    let input = PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap();
    let publication = Publication::reserve(&root, input).unwrap();
    assert_eq!(
        std::fs::metadata(root.join("codex-home/auth.json"))
            .unwrap()
            .len(),
        0
    );
    let record = publication.record();
    let metadata = serde_json::to_string(&record).unwrap();
    assert!(!metadata.contains("sk-private"));
    let _held = publication.publish(Path::new("/launch/workspace")).unwrap();
    assert!(!root.join("codex-home/config.toml").exists());
    let root_fd = File::open(&root).unwrap();
    verify_record(&root_fd, &record, true).unwrap();
    std::fs::write(dir.path().join("source/record.json"), b"changed record").unwrap();
    assert!(verify_record(&root_fd, &record, true).is_err());
    // Matching stopped cleanup may still verify the derived file independently.
    verify_record(&root_fd, &record, false).unwrap();
}
#[test]
fn publication_refuses_changed_original_bytes_without_writing_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (fd, digest, bytes, record) = source(dir.path());
    let root = root(dir.path());
    let publication = Publication::reserve(
        &root,
        PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("source/material.bin"), b"changed key").unwrap();
    assert!(publication.publish(Path::new("/launch/workspace")).is_err());
    assert_eq!(
        std::fs::metadata(root.join("codex-home/auth.json"))
            .unwrap()
            .len(),
        0
    );
}
#[test]
fn symlink_replacement_and_nonprivate_auth_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let (fd, digest, bytes, record) = source(dir.path());
    let root = root(dir.path());
    let publication = Publication::reserve(
        &root,
        PrivateLaunchInput::codex_api_key(fd, digest, bytes, record).unwrap(),
    )
    .unwrap();
    let record = publication.record();
    let _held = publication.publish(Path::new("/launch/workspace")).unwrap();
    std::fs::set_permissions(
        root.join("codex-home/auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(verify_record(&File::open(&root).unwrap(), &record, false).is_err());
    std::fs::remove_file(root.join("codex-home/auth.json")).unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("source/material.bin"),
        root.join("codex-home/auth.json"),
    )
    .unwrap();
    assert!(verify_record(&File::open(&root).unwrap(), &record, false).is_err());
    assert_eq!(
        std::fs::read(dir.path().join("source/material.bin")).unwrap(),
        b"sk-private-offline-only"
    );
}
#[test]
fn scoped_auth_contains_only_fresh_capability_and_rechecks_original() {
    let dir = tempfile::tempdir().unwrap();
    let (fd, digest, bytes, record_digest) = source(dir.path());
    let root = root(dir.path());
    let (scope, _) = scoped();
    let cap = Zeroizing::new(format!("aq-responses-{}", "a".repeat(64)).into_bytes());
    let input = PrivateLaunchInput::codex_scoped_capability(
        fd,
        OriginalPrivateMaterial {
            digest,
            bytes,
            record_digest,
        },
        scope,
        cap.clone(),
    )
    .unwrap();
    let publication = Publication::reserve(&root, input).unwrap();
    assert_eq!(
        std::fs::metadata(root.join("codex-home/auth.json"))
            .unwrap()
            .len(),
        0
    );
    let record = publication.record();
    let metadata = serde_json::to_string(&record).unwrap();
    assert!(!metadata.contains("sk-private"));
    assert!(!metadata.contains(std::str::from_utf8(&cap).unwrap()));
    let _source = publication.publish(Path::new("/launch/workspace")).unwrap();
    let auth = std::fs::read(root.join("codex-home/auth.json")).unwrap();
    assert_eq!(auth, *derive_auth(&cap).unwrap());
    assert!(!String::from_utf8_lossy(&auth).contains("sk-private"));
    verify_record(&File::open(&root).unwrap(), &record, true).unwrap();
    std::fs::write(
        dir.path().join("source/record.json"),
        b"changed original record",
    )
    .unwrap();
    assert!(verify_record(&File::open(&root).unwrap(), &record, true).is_err());
    verify_record(&File::open(&root).unwrap(), &record, false).unwrap();
}
fn proxy_route(origin: &str) -> Vec<String> {
    vec![
        "--config".into(),
        "model_provider=\"aq-runner-proxy\"".into(),
        "--config".into(),
        format!("model_providers.aq-runner-proxy.base_url=\"{origin}/v1\""),
        "--config".into(),
        "model_providers.aq-runner-proxy.wire_api=\"responses\"".into(),
    ]
}
#[test]
fn scoped_auth_requires_exact_launch_manifest_deadline_and_single_route() {
    let (scope, mut config) = scoped_launch_fixture();
    scope.verify_launch(&config).unwrap();
    let route = config.command.arguments.clone();
    // A duplicated provider base URL is refused.
    config.command.arguments.extend([
        "--config".into(),
        format!(
            "model_providers.aq-runner-proxy.base_url=\"{}/v1\"",
            scope.origin
        ),
    ]);
    assert!(scope.verify_launch(&config).is_err());
    // The legacy top-level route is refused alongside the provider route.
    config.command.arguments = route.clone();
    config.command.arguments.extend([
        "--config".into(),
        format!("openai_base_url=\"{}/v1\"", scope.origin),
    ]);
    assert!(scope.verify_launch(&config).is_err());
    // The legacy top-level route alone is no longer an accepted route.
    config.command.arguments = vec![
        "--config".into(),
        format!("openai_base_url=\"{}/v1\"", scope.origin),
    ];
    assert!(scope.verify_launch(&config).is_err());
    // Each provider setting is required: selector, base URL and wire API.
    for i in (1..route.len()).step_by(2) {
        let mut partial = route.clone();
        partial.drain(i - 1..=i);
        config.command.arguments = partial;
        assert!(
            scope.verify_launch(&config).is_err(),
            "missing {}",
            route[i]
        );
    }
    // A different provider selection or origin is refused.
    config.command.arguments = route.clone();
    config.command.arguments[1] = "model_provider=\"openai\"".into();
    assert!(scope.verify_launch(&config).is_err());
    config.command.arguments = proxy_route("http://127.0.0.1:1");
    assert!(scope.verify_launch(&config).is_err());
}

#[test]
fn scoped_auth_refuses_foreign_permit_manifest_or_origin() {
    let (scope, mut config) = scoped_launch_fixture();

    config.permit.expires_boot_ms += 1;
    assert!(scope.verify_launch(&config).is_err());
    config.permit.expires_boot_ms -= 1;
    config.launch_id = Uuid::new_v4();
    assert!(scope.verify_launch(&config).is_err());
    config.launch_id = scope.launch_id;
    if let LaunchDocument::Task(m) = &mut config.manifest {
        m.model.max_output_tokens += 1;
    }
    assert!(scope.verify_launch(&config).is_err());
    let mut foreign = scope.clone();
    foreign.origin = "https://provider.example".into();
    assert!(foreign.validate().is_err());
}

fn reserve_test_login(dir: &Path) -> (PathBuf, Publication, Vec<u8>) {
    let (source, _, _, record) = source(dir);
    let bytes = login("offline-refresh");
    std::fs::write(dir.join("source/material.bin"), &bytes).unwrap();
    let (scope, _) = scoped();
    let input = PrivateLaunchInput::codex_chatgpt_auth_json(
        source,
        OriginalPrivateMaterial {
            digest: Digest::of(&bytes),
            bytes: bytes.len() as u64,
            record_digest: record,
        },
        scope.binding,
        scope.launch_id,
        scope.deadline_boot_ms,
    )
    .unwrap();
    let root = root(dir);
    let publication = Publication::reserve(&root, input).unwrap();
    (root, publication, bytes)
}

fn assert_trusted_configuration(root: &Path, workspace: &Path, bytes: &[u8]) {
    let config_path = root.join("codex-home/config.toml");
    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let projects = config["projects"].as_table().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[workspace.to_str().unwrap()]["trust_level"].as_str(),
        Some("trusted")
    );
    assert_eq!(
        std::fs::metadata(config_path).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read(root.join("codex-home/auth.json")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::metadata(root.join("codex-home/auth.json"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}

fn scoped_launch_fixture() -> (ScopedCodexAuthBinding, SealedLaunch) {
    let (scope, manifest) = scoped();
    let config = SealedLaunch {
        launch_id: scope.launch_id,
        permit: ExecutionPermit {
            session: manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: 1,
            expires_boot_ms: scope.deadline_boot_ms,
        },
        resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
        manifest: manifest.into(),
        workspace: "/work".into(),
        helper: "/helper".into(),
        tmux: "/usr/bin/tmux".into(),
        tmux_version: "tmux 3.4".into(),
        resource_qualification: None,
        rendered_startup: None,
        private_input: None,
        observed: None,
        agent_identity: None,
        command: LocalCommand {
            executable: "/opt/codex".into(),
            arguments: proxy_route(&scope.origin),
            environment: Default::default(),
        },
    };
    (scope, config)
}
