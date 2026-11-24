#[tokio::test]
async fn launch_policy_is_fixed() {
    let mut h = harness().await;
    let request = request(1, 2, 3).with_mount_aliases(vec!["repo-cache".into(), "scratch".into()]);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    assert_eq!(sandbox.backend_id, request.name);
    assert_eq!(sandbox.kind, BackendKind::DockerLocal);
    assert!(
        posts(&h.docker).contains(&format!("/v1.44/containers/create?name={}", request.name)),
        "{:?}",
        h.docker.requests()
    );
    let body = h.docker.container(&request.name).unwrap().body;
    assert_eq!(
        body["HostConfig"],
        json!({
            "Privileged": false,
            "CapAdd": [],
            "CapDrop": ["ALL"],
            "Devices": [],
            "DeviceCgroupRules": [],
            "SecurityOpt": ["no-new-privileges"],
            "NanoCpus": 500_000_000u64,
            "Memory": 64 * 1024 * 1024,
            "MemorySwap": 64 * 1024 * 1024,
            "PidsLimit": 32,
            "NetworkMode": "bridge",
            "PortBindings": {},
            "PublishAllPorts": false,
            "Binds": [],
            "Mounts": [
                {"Type": "volume", "Source": format!("{}-work", request.name), "Target": "/aq/work", "ReadOnly": false},
                {"Type": "bind", "Source": h.cache.path().to_str().unwrap(), "Target": "/aq/cache", "ReadOnly": true},
                {"Type": "bind", "Source": h.scratch.path().to_str().unwrap(), "Target": "/aq/scratch", "ReadOnly": false},
            ],
            "IpcMode": "private",
            "RestartPolicy": {"Name": "no"},
            "AutoRemove": false,
        })
    );
    assert_eq!(body["Labels"], expected_labels(&request));
    assert_eq!(body["Image"], image());
    assert_eq!(body["Env"], json!([]));
    assert_eq!(body["Cmd"], json!(["/aq/bin/runner"]));
    assert_eq!(body["WorkingDir"], "/aq/work");
    assert_eq!(body["ExposedPorts"], json!({}));
    for key in ["AttachStdin", "OpenStdin", "Tty"] {
        assert_eq!(body[key], false, "{key}");
    }
    let volume = h.docker.volume(&format!("{}-work", request.name)).unwrap();
    assert_eq!(volume.labels, expected_labels(&request));

    let mut bare = harness_with(ScriptedDocker::new(), |config| config.runner_command = None).await;
    let request = self::request(1, 2, 4);
    bounded(bare.backend.create(&request)).await.unwrap();
    let body = bare.docker.container(&request.name).unwrap().body;
    assert!(
        body.get("Cmd").is_none(),
        "image default command when unset"
    );
    drop(h.socket);
}

/// Spec 0006 §5.1: the agent channel is a fixed mount kind. The source is
/// always `<channel_root>/<attempt>` (the request only says "yes"), the target
/// always `/aq/channel`, read-write; no channel directory means no create, and
/// a request without the channel hashes exactly as before.
#[tokio::test]
async fn agent_channel_mount_is_fixed_to_the_attempt_directory() {
    let channels = tempfile::tempdir().unwrap();
    let root = channels.path().to_owned();
    let mut h = harness_with(ScriptedDocker::new(), |c| c.channel_root = Some(root)).await;
    let request = request(1, 2, 3).with_agent_channel();
    assert_eq!(
        bounded(h.backend.create(&request)).await,
        Err(BackendError::InvalidRequest(
            "agent channel directory absent (agent_channel_unavailable)"
        ))
    );
    let source = channels.path().join(Uuid::from_u128(3).to_string());
    std::fs::create_dir(&source).unwrap();
    bounded(h.backend.create(&request)).await.unwrap();
    let mounts = h.docker.container(&request.name).unwrap().body["HostConfig"]["Mounts"].clone();
    assert_eq!(
        mounts[1],
        json!({"Type": "bind", "Source": source.to_str().unwrap(), "Target": "/aq/channel", "ReadOnly": false})
    );
    assert_eq!(mounts.as_array().unwrap().len(), 2);
    assert!(
        !serde_json::to_string(&self::request(1, 2, 3))
            .unwrap()
            .contains("agent_channel")
    );

    let mut bare = harness().await;
    assert!(
        bounded(
            bare.backend
                .create(&self::request(1, 2, 4).with_agent_channel())
        )
        .await
        .is_err(),
        "no channel root configured"
    );
    let socket = owned_socket();
    let (cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for edit in [
        |c: &mut DockerLocalConfig| {
            c.mount_aliases.get_mut("scratch").unwrap().target = "/aq/channel".into()
        },
        |c: &mut DockerLocalConfig| {
            c.mount_aliases.get_mut("scratch").unwrap().target = "/aq/channel/x".into()
        },
        |c: &mut DockerLocalConfig| c.material_path = "/aq/channel/material".into(),
        |c: &mut DockerLocalConfig| c.channel_root = Some("relative".into()),
    ] {
        let mut config = base_config(&socket, &cache, &scratch);
        edit(&mut config);
        assert!(config.validate().is_err());
    }
}

/// The transcript mount is a fixed mount kind: the node's own 0700 directory at the
/// harness's transcript directory, read-write, and nothing else of the harness home.
#[tokio::test]
async fn transcript_mount_is_only_the_harness_transcript_directory() {
    use aq_worker::sandbox_driver::DriverHarness;
    use std::os::unix::fs::PermissionsExt;
    let slot = tempfile::tempdir().unwrap();
    let host = slot.path().join("transcript");
    std::fs::create_dir(&host).unwrap();
    let mode = |mode| std::fs::set_permissions(&host, std::fs::Permissions::from_mode(mode));
    for (attempt, harness) in [(3, DriverHarness::Claude), (4, DriverHarness::Codex)] {
        let target = harness.transcript_dir();
        assert!(target.starts_with("/aq/home/") && !target.starts_with("/aq/work"));
        let mount = Some((host.clone(), target.to_owned()));
        let mut h = harness_with(ScriptedDocker::new(), |c| c.transcript = mount).await;
        let request = request(1, 2, attempt);
        mode(0o750).unwrap();
        assert_eq!(
            bounded(h.backend.create(&request)).await,
            Err(BackendError::InvalidRequest(
                "transcript directory must be the node's own 0700 directory"
            ))
        );
        mode(0o700).unwrap();
        bounded(h.backend.create(&request)).await.unwrap();
        let body = h.docker.container(&request.name).unwrap().body;
        let mounts = body["HostConfig"]["Mounts"].as_array().unwrap();
        assert_eq!(
            mounts[1..],
            [
                json!({"Type": "bind", "Source": host.to_str().unwrap(), "Target": target, "ReadOnly": false})
            ]
        );
        assert_eq!(mounts[0]["Target"], "/aq/work");
        // The request, and so the sandbox's name binding, does not carry the mount.
        assert_eq!(body["Labels"], expected_labels(&request));
    }
    let socket = owned_socket();
    let (cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let claude = DriverHarness::Claude.transcript_dir();
    let refused: [&dyn Fn(&mut DockerLocalConfig); 7] = [
        // The whole private harness home, a parent, a child, the workspace.
        &|c| c.transcript.as_mut().unwrap().1 = "/aq/home".into(),
        &|c| c.transcript.as_mut().unwrap().1 = "/aq/home/codex".into(),
        &|c| c.transcript.as_mut().unwrap().1 = format!("{claude}/x"),
        &|c| c.transcript.as_mut().unwrap().1 = "/aq/work/transcript".into(),
        &|c| c.transcript.as_mut().unwrap().0 = "relative".into(),
        &|c| c.mount_aliases.get_mut("scratch").unwrap().target = "/aq/home".into(),
        &|c| c.mount_aliases.get_mut("scratch").unwrap().host_path = slot.path().into(),
    ];
    for edit in refused {
        let mut config = base_config(&socket, &cache, &scratch);
        config.transcript = Some((host.clone(), claude.into()));
        config.validate().unwrap();
        edit(&mut config);
        assert!(config.validate().is_err());
    }
}

/// Finding 8 (PR #43 review): a mount alias whose host path equals, contains
/// or sits under the channel root would expose other attempts' sockets. It is
/// refused at config load, through a symlink too and before the root exists.
#[test]
fn a_mount_alias_never_overlaps_the_channel_root() {
    let socket = owned_socket();
    let (cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let host = tempfile::tempdir().unwrap();
    let root = host.path().join("channels"); // not created yet
    let link = host.path().join("link-to-host");
    std::os::unix::fs::symlink(host.path(), &link).unwrap();
    let mut config = base_config(&socket, &cache, &scratch);
    config.channel_root = Some(root.clone());
    config.validate().unwrap();
    config.check_channel_root(&root).unwrap();
    for overlapping in [
        root.clone(),
        host.path().to_owned(),
        root.join(Uuid::from_u128(3).to_string()),
        link.clone(),
        link.join("channels").join("x"),
    ] {
        let mut config = base_config(&socket, &cache, &scratch);
        config.mount_aliases.get_mut("scratch").unwrap().host_path = overlapping.clone();
        let refused = Err(BackendError::InvalidRequest(
            "channel root must be absolute, expose no daemon socket and overlap no mount alias",
        ));
        assert_eq!(config.check_channel_root(&root), refused, "{overlapping:?}");
        config.channel_root = Some(root.clone());
        assert_eq!(config.validate(), refused, "{overlapping:?}");
    }
}

/// Finding 1 (PR #43 review): the channel accepts exactly the enrollment
/// origins the node's own native transport accepts for its credential.
#[test]
fn the_channel_accepts_the_origins_the_native_transport_accepts() {
    for origin in [
        "https://aq.example.com",
        "https://10.0.0.5",
        "http://127.0.0.1:32839",
        "http://127.0.0.1",
        "http://127.0.0.2:8080",
        "http://[::1]:8080",
        "http://host.docker.internal:32839",
        "http://localhost:8080",
        "http://aq-server:8080",
        "http://192.168.1.5:8080",
        "http://0.0.0.0:8080",
        "ftp://aq.example.com",
        "https://user:pw@aq.example.com",
    ] {
        let worker = aq_worker::config::WorkerConfig {
            server_url: origin.into(),
            node_id: Uuid::from_u128(1),
            credential: "node-credential-0123456789abcdef".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        assert_eq!(
            aq_agent_channel::node_origin(origin).is_ok(),
            aq_worker::native::http::NativeClient::from_config(&worker).is_ok(),
            "{origin}"
        );
    }
}

#[tokio::test]
async fn refuses_unallowlisted_template() {
    let mut h = harness().await;
    let mut request = request(1, 2, 3);
    request.template_digest = TemplateDigest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
    assert_eq!(
        bounded(h.backend.create(&request)).await,
        Err(NOT_ALLOWLISTED)
    );
    assert_eq!(
        bounded(h.backend.pull_template(&request.template_digest)).await,
        Err(NOT_ALLOWLISTED)
    );
    assert!(h.docker.requests().is_empty());
}

#[tokio::test]
async fn pull_template_proves_presence() {
    let mut h = harness().await;
    bounded(h.backend.pull_template(&digest())).await.unwrap();
    let requests: Vec<String> = h.docker.requests().into_iter().map(|(_, p)| p).collect();
    assert_eq!(
        requests,
        vec![
            format!(
                "/v1.44/images/create?fromImage=docker.io%2Flibrary%2Fbusybox%40sha256%3A{HEX}"
            ),
            format!("/v1.44/images/{}/json", image()),
        ]
    );
    let mut fresh = harness().await;
    fresh.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: "/v1.44/images/create".into(),
        effect: Effect::LieOkWithoutApply,
    });
    assert_eq!(
        bounded(fresh.backend.pull_template(&digest())).await,
        Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        "a pull that reported success without the image is not proof"
    );
}

#[tokio::test]
async fn refuses_missing_resources() {
    let mut h = harness().await;
    let request = CreateRequest::new(
        owner(1, 2, 3),
        digest(),
        3_600_000,
        vec![Capability::PersistentDiskDuringAttempt],
    );
    assert_eq!(
        bounded(h.backend.create(&request)).await,
        Err(BackendError::InvalidRequest(
            "docker-local requires reservation limits"
        ))
    );
    assert!(h.docker.requests().is_empty());
}

#[tokio::test]
async fn refuses_undeclared_mount_alias() {
    let mut h = harness().await;
    let undeclared = request(1, 2, 3).with_mount_aliases(vec!["home".into()]);
    assert_eq!(
        bounded(h.backend.create(&undeclared)).await,
        Err(BackendError::InvalidRequest("undeclared mount alias"))
    );
    let duplicate =
        request(1, 2, 3).with_mount_aliases(vec!["repo-cache".into(), "repo-cache".into()]);
    assert!(matches!(
        bounded(h.backend.create(&duplicate)).await,
        Err(BackendError::InvalidRequest(_))
    ));
    assert!(h.docker.requests().is_empty());
}

#[tokio::test]
async fn refuses_unsupported_capabilities() {
    let mut h = harness().await;
    for capability in [
        Capability::VmIsolation,
        Capability::NestedDocker,
        Capability::SuspendSnapshot,
    ] {
        let mut request = request(1, 2, 3);
        request.required_capabilities.push(capability);
        assert_eq!(
            bounded(h.backend.create(&request)).await,
            Err(BackendError::UnsupportedCapability(capability))
        );
    }
    assert!(h.docker.requests().is_empty());
}

#[tokio::test]
async fn advertises_no_vm_nested_or_snapshot() {
    let h = harness().await;
    assert_eq!(h.backend.kind(), BackendKind::DockerLocal);
    assert_eq!(
        h.backend.capabilities(),
        vec![Capability::PersistentDiskDuringAttempt]
    );
}
