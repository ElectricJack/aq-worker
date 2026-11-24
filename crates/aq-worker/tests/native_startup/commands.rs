#[test]
fn exact_offer_maps_closed_argv_and_delivers_one_payload_without_secret_metadata() {
    let (root, home, work) = dirs();
    let (offer, payload) = fixture();
    let digest = canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer, &offer.startup.renderer_qualification);
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
        .unwrap();
    let command = prepared.command();
    assert_eq!(command.arguments.last().unwrap(), "-");
    assert_eq!(
        command
            .arguments
            .iter()
            .filter(|s| s.as_str() == "-")
            .count(),
        1
    );
    assert!(command.arguments.iter().any(|s| s == "--no-daemon"));
    assert!(
        command
            .arguments
            .iter()
            .any(|s| s == "web_search=\"disabled\"")
    );
    for setting in [
        "shell_environment_policy.inherit=\"none\"",
        "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin\"",
        "features.shell_snapshot=false",
        "features.shell_snapshot_v2=false",
        "allow_login_shell=false",
        "features.plugins=false",
        "features.recommended_plugins=false",
        "features.remote_plugin=false",
        "features.plugin_discovery_prefetch=false",
        "features.apps=false",
        "features.memory_tool=false",
        "memories.generate_memories=false",
    ] {
        assert!(command.arguments.iter().any(|argument| argument == setting));
    }
    assert!(
        !serde_json::to_string(command)
            .unwrap()
            .contains(std::str::from_utf8(&payload).unwrap())
    );
    assert_eq!(
        command.environment.keys().collect::<Vec<_>>(),
        vec!["CODEX_HOME"]
    );
    let mut out = Vec::new();
    prepared.write_startup(&mut out).unwrap();
    assert_eq!(out, payload);
}
#[test]
fn tool_environment_config_starts_empty_and_adds_only_fixed_template_defaults() {
    let (root, home, work) = dirs();
    let (offer, payload) = fixture();
    let digest = canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer, &offer.startup.renderer_qualification);
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
        .unwrap();
    let config = prepared
        .command()
        .arguments
        .windows(2)
        .filter(|pair| pair[0] == "--config" && pair[1].starts_with("shell_environment_policy."))
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let config: toml::Value = toml::from_str(&config).unwrap();
    let policy = config["shell_environment_policy"].as_table().unwrap();
    assert_eq!(policy.len(), 3);
    assert_eq!(policy["inherit"].as_str(), Some("none"));
    assert_eq!(policy["experimental_use_profile"].as_bool(), Some(false));
    let set = policy["set"].as_table().unwrap();
    assert_eq!(set.len(), 4);
    assert_eq!(set["PATH"].as_str(), Some("/usr/local/bin:/usr/bin:/bin"));
    assert_eq!(set["CARGO_INCREMENTAL"].as_str(), Some("0"));
    assert_eq!(
        set["CARGO_PROFILE_DEV_DEBUG"].as_str(),
        Some("line-tables-only")
    );
    assert_eq!(
        set["CARGO_PROFILE_TEST_DEBUG"].as_str(),
        Some("line-tables-only")
    );
}
