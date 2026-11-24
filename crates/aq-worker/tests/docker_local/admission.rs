#[test]
fn refuses_main_docker_socket() {
    for main in MAIN_DAEMON_SOCKETS {
        assert_eq!(
            DockerLocalConfig::new(PathBuf::from(main)).validate(),
            Err(REFUSE_MAIN)
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("innocent.sock");
    std::os::unix::fs::symlink("/var/run/docker.sock", &link).unwrap();
    let via_dir = dir.path().join("run");
    std::os::unix::fs::symlink("/var/run", &via_dir).unwrap();
    for path in [link, via_dir.join("docker.sock")] {
        let result = DockerLocalConfig::new(path).validate();
        if std::path::Path::new("/var/run/docker.sock").exists() {
            assert_eq!(result, Err(REFUSE_MAIN));
        } else {
            assert!(result.is_err(), "dangling main-daemon symlink accepted");
        }
    }
}

#[test]
fn refuses_socket_not_owned_by_current_user() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let root_socket = [
        "/run/dbus/system_bus_socket",
        "/run/systemd/private",
        "/run/systemd/notify",
        "/run/udev/control",
    ]
    .into_iter()
    .find(|path| std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket() && m.uid() == 0))
    .expect("no root-owned socket available on this host to test the owner check");
    assert_eq!(
        DockerLocalConfig::new(PathBuf::from(root_socket)).validate(),
        Err(BackendError::InvalidRequest(
            "sandbox daemon socket is not owned by the worker user"
        ))
    );
    let socket = owned_socket();
    assert_eq!(
        DockerLocalConfig::new(socket.path.clone()).validate(),
        Ok(())
    );
}

#[test]
fn refuses_unsafe_config() {
    let socket = owned_socket();
    let base = DockerLocalConfig::new(socket.path.clone());
    let digest = TemplateDigest::parse(format!("sha256:{}", "b".repeat(64))).unwrap();
    let mut tagged = base.clone();
    tagged.templates.push(AllowedTemplate {
        image: "docker.io/library/busybox:latest".into(),
        digest: digest.clone(),
    });
    assert!(tagged.validate().is_err());
    let alias = |host: &str, target: &str| {
        let mut config = base.clone();
        config.mount_aliases.insert(
            "repo-cache".into(),
            MountAlias {
                host_path: host.into(),
                target: target.into(),
                read_only: true,
            },
        );
        config.validate()
    };
    let host = socket._dir.path().to_str().unwrap().to_string();
    let cache = tempfile::tempdir().unwrap();
    let cache = cache.path().to_str().unwrap();
    assert!(alias("relative/path", "/aq/cache").is_err());
    assert!(alias("/tmp", "/etc/cache").is_err());
    assert!(alias("/tmp", "/aq/../etc").is_err());
    assert!(alias("/tmp", "/aq/work/cache").is_err());
    assert!(alias("/tmp", "/aq/private/cache").is_err());
    assert!(alias("/var/run/docker.sock", "/aq/cache").is_err());
    assert!(
        alias(&host, "/aq/cache").is_err(),
        "parent of the daemon socket"
    );
    assert!(
        alias("/", "/aq/cache").is_err(),
        "ancestor of the daemon socket"
    );
    assert_eq!(alias(cache, "/aq/cache"), Ok(()));
}

#[tokio::test]
async fn refuses_non_rootless_daemon() {
    let socket = owned_socket();
    let config = DockerLocalConfig::new(socket.path.clone());
    let result = bounded(DockerLocalBackend::connect(
        config.clone(),
        ScriptedDocker::new().rootless(false),
    ))
    .await;
    assert_eq!(
        result.err(),
        Some(BackendError::InvalidRequest(
            "sandbox daemon must be rootless"
        ))
    );
    assert!(
        bounded(DockerLocalBackend::connect(config, ScriptedDocker::new()))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn refuses_daemon_that_drops_resource_limits() {
    const NO_DRIVER: BackendError = BackendError::InvalidRequest(
        "sandbox daemon reports no cgroup driver; resource limits would be silently dropped",
    );
    const NOT_V2: BackendError = BackendError::InvalidRequest(
        "sandbox daemon must use cgroup v2; resource limits would be silently dropped",
    );
    let socket = owned_socket();
    let config = DockerLocalConfig::new(socket.path.clone());
    let cases = [
        (Some("systemd"), Some("2"), None),
        (Some("cgroupfs"), Some("2"), None),
        // docker:dind-rootless
        (Some("none"), Some("2"), Some(NO_DRIVER)),
        (None, Some("2"), Some(NO_DRIVER)),
        (Some("systemd"), Some("1"), Some(NOT_V2)),
        (Some("cgroupfs"), Some("1"), Some(NOT_V2)),
        (Some("systemd"), None, Some(NOT_V2)),
    ];
    for (driver, version, expected) in cases {
        let result = bounded(DockerLocalBackend::connect(
            config.clone(),
            ScriptedDocker::new().cgroup(driver, version),
        ))
        .await;
        assert_eq!(
            result.err(),
            expected,
            "driver {driver:?} version {version:?}"
        );
    }
}

#[tokio::test]
async fn connect_times_out_as_uncertain() {
    let socket = owned_socket();
    let mut config = DockerLocalConfig::new(socket.path.clone());
    config.api_timeout_ms = 100;
    let docker = ScriptedDocker::new();
    docker.pause();
    let result = bounded(DockerLocalBackend::connect(config, docker.clone())).await;
    assert_eq!(
        result.err(),
        Some(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    docker.resume();
}

#[test]
fn confinement_is_container_only() {
    assert_eq!(CONFINEMENT, &[ConfinementMode::Container]);
    assert_eq!(require_confinement(ConfinementMode::Container), Ok(()));
    assert_eq!(
        require_confinement(ConfinementMode::Native),
        Err(BackendError::UnsupportedOperation)
    );
}
