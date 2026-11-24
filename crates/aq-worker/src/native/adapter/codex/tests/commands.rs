fn normalized_recipe_golden(scoped: bool) -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (offer, payload) = fixture();
    let digest = crate::native::journal::canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer);
    let mut prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
        .unwrap();
    if scoped {
        prepared = prepared
            .with_scoped_provider("http://127.0.0.1:32123")
            .unwrap();
    }
    let mut bytes = serde_json::to_string_pretty(prepared.command())
        .unwrap()
        .replace(root.path().to_str().unwrap(), "<fixture>")
        .into_bytes();
    bytes.push(b'\n');
    bytes
}
#[test]
fn extracted_mapper_preserves_complete_base_command_golden() {
    assert_eq!(
        normalized_recipe_golden(false),
        include_bytes!("../goldens/base.json")
    );
}
#[test]
fn extracted_mapper_preserves_complete_scoped_command_golden() {
    assert_eq!(
        normalized_recipe_golden(true),
        include_bytes!("../goldens/scoped.json")
    );
}
#[test]
fn scoped_provider_routes_through_runner_proxy_provider() {
    let command: serde_json::Value =
        serde_json::from_slice(&normalized_recipe_golden(true)).unwrap();
    let arguments: Vec<&str> = command["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(!arguments.iter().any(|a| a.starts_with("openai_base_url=")));
    for expected in [
        "model_provider=\"aq-runner-proxy\"",
        "model_providers.aq-runner-proxy.base_url=\"http://127.0.0.1:32123/v1\"",
        "model_providers.aq-runner-proxy.wire_api=\"responses\"",
        "cli_auth_credentials_store=\"file\"",
    ] {
        assert_eq!(
            arguments.iter().filter(|a| **a == expected).count(),
            1,
            "{expected}"
        );
    }
    // Stdin prompt marker stays last, after every override.
    assert_eq!(arguments.last(), Some(&"-"));
}
#[test]
fn scoped_provider_retains_closed_origin_validation() {
    assert_eq!(
        scoped_origin("http://127.0.0.1:32123/").unwrap(),
        "http://127.0.0.1:32123"
    );
    for origin in [
        "http://localhost:32123",
        "https://127.0.0.1:32123",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:32123/v1",
        "http://user@127.0.0.1:32123",
        "http://127.0.0.1:32123?query=1",
        "http://127.0.0.1:32123#fragment",
        "http://192.0.2.1:32123",
    ] {
        assert!(
            scoped_origin(origin).is_err(),
            "unexpected origin acceptance: {origin}"
        );
    }
}
