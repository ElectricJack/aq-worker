// SBX20: per-sandbox disk limit.
const SHARED_ROOT: BackendError = BackendError::InvalidRequest(
    "sandbox daemon storage shares the root filesystem; set shared_root_fs to override",
);

#[tokio::test]
async fn disk_admission_refuses_shared_root_fs_unless_overridden() {
    let socket = owned_socket();
    let cache = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for root in [Some("/"), None] {
        let docker = ScriptedDocker::new().root_dir(root);
        let refused = bounded(DockerLocalBackend::connect(
            base_config(&socket, &cache, &scratch),
            docker,
        ))
        .await
        .err();
        assert_eq!(refused, Some(SHARED_ROOT), "{root:?}");
    }
    let mut config = base_config(&socket, &cache, &scratch);
    config.shared_root_fs = true;
    let backend = bounded(DockerLocalBackend::connect(
        config,
        ScriptedDocker::new().root_dir(Some("/")),
    ))
    .await
    .unwrap();
    let status = backend.disk_limit();
    assert!(status.shared_root_fs, "the override is reported");
    assert_eq!(status.mode, Some(DiskLimitMode::Watchdog));
    assert_eq!(status.watch_interval_ms, 10_000);

    // A distinct filesystem (the double reports /proc) needs no override.
    let h = harness().await;
    assert!(!h.backend.disk_limit().shared_root_fs);
    assert_eq!(
        h.backend.disk_limit().mode.map(DiskLimitMode::capability),
        Some("disk-limit=watchdog")
    );
}

#[tokio::test]
async fn hard_disk_limit_carries_storage_size_only_in_hard_mode() {
    // Watchdog (default): neither the container nor the volume is sized.
    let mut h = harness().await;
    let soft = request(1, 2, 3);
    bounded(h.backend.create(&soft)).await.unwrap();
    let body = h.docker.container(&soft.name).unwrap().body;
    assert!(body["HostConfig"].get("StorageOpt").is_none());
    let volume = format!("{}-work", soft.name);
    assert_eq!(h.docker.volume_driver_opts(&volume), None);

    // Hard mode needs XFS-backed storage.
    let socket = owned_socket();
    let cache = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut config = base_config(&socket, &cache, &scratch);
    config.hard_disk_limit = true;
    assert_eq!(
        bounded(DockerLocalBackend::connect(config, ScriptedDocker::new()))
            .await
            .err(),
        Some(BackendError::InvalidRequest(
            "hard disk limit requires XFS (prjquota) sandbox storage"
        ))
    );

    let mut h = harness_with(ScriptedDocker::new().backing_fs("xfs"), |c| {
        c.hard_disk_limit = true;
    })
    .await;
    assert_eq!(
        h.backend.disk_limit().mode.map(DiskLimitMode::capability),
        Some("disk-limit=hard")
    );
    let limit = 5 * 1024 * 1024 * 1024u64;
    let hard = request(1, 2, 4).with_resources(ResourceLimits {
        disk_bytes: Some(limit),
        ..LIMITS
    });
    bounded(h.backend.create(&hard)).await.unwrap();
    let body = h.docker.container(&hard.name).unwrap().body;
    assert_eq!(
        body["HostConfig"]["StorageOpt"],
        serde_json::json!({ "size": limit.to_string() })
    );
    assert_eq!(
        h.docker.volume_driver_opts(&format!("{}-work", hard.name)),
        Some(serde_json::json!({ "o": format!("size={limit}") }))
    );
    // Without a reservation value the node default (32 GB) applies.
    let default = request(1, 2, 5);
    bounded(h.backend.create(&default)).await.unwrap();
    assert_eq!(
        h.docker.container(&default.name).unwrap().body["HostConfig"]["StorageOpt"]["size"],
        DEFAULT_DISK_LIMIT_BYTES.to_string()
    );
}

#[tokio::test]
async fn disk_watchdog_sampling_flags_only_the_sandbox_over_its_limit() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::StaysRunning), |_| {}).await;
    let limit = 1024 * 1024u64;
    let sized = |attempt| {
        request(1, 2, attempt).with_resources(ResourceLimits {
            disk_bytes: Some(limit),
            ..LIMITS
        })
    };
    let (over, under) = (sized(3), sized(4));
    let over_sandbox = bounded(h.backend.create(&over)).await.unwrap();
    let under_sandbox = bounded(h.backend.create(&under)).await.unwrap();
    bounded(h.backend.start_runner(&over_sandbox))
        .await
        .unwrap();
    bounded(h.backend.start_runner(&under_sandbox))
        .await
        .unwrap();
    // Writable layer plus work volume together cross the limit.
    h.docker.set_container_size(&over.name, limit / 2);
    h.docker
        .set_volume_size(&format!("{}-work", over.name), (limit / 2) as i64);
    h.docker.set_container_size(&under.name, limit / 4);
    // An unsized volume (-1) contributes nothing.
    h.docker
        .set_volume_size(&format!("{}-work", under.name), -1);

    let mut usage = bounded(h.backend.disk_usage(Uuid::from_u128(1)))
        .await
        .unwrap();
    usage.sort_by_key(|u| u.sandbox.backend_id.clone());
    assert_eq!(usage.len(), 2);
    let flagged: Vec<_> = usage.iter().filter(|u| u.exceeded()).collect();
    assert_eq!(flagged.len(), 1);
    assert_eq!(flagged[0].sandbox, over_sandbox);
    assert_eq!(flagged[0].used_bytes, limit);
    assert_eq!(flagged[0].limit_bytes, limit);
    let other = usage.iter().find(|u| u.sandbox == under_sandbox).unwrap();
    assert_eq!(other.used_bytes, limit / 4);
    // Another node's watchdog sees nothing of ours.
    assert!(
        bounded(h.backend.disk_usage(Uuid::from_u128(9)))
            .await
            .unwrap()
            .is_empty()
    );

    // The controller's response: stop (never destroy) the offender only.
    assert!(bounded(h.backend.stop(&over_sandbox)).await.is_confirmed());
    assert!(deletes(&h.docker).is_empty());
    let stopped = bounded(h.backend.inspect(&over_sandbox)).await.unwrap();
    assert_eq!(stopped.state, SandboxState::Stopped);
    assert!(stopped.disk_present);
    assert_eq!(
        bounded(h.backend.inspect(&under_sandbox))
            .await
            .unwrap()
            .state,
        SandboxState::Running
    );
}

/// Fake statvfs: records the measured path and reports fixed blocks.
struct FakeStatvfs(std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>);
impl Statvfs for FakeStatvfs {
    fn statvfs(&self, path: &std::path::Path) -> std::io::Result<FsStats> {
        self.0.lock().unwrap().push(path.to_path_buf());
        Ok(FsStats::from_blocks(4096, 1_000, 300, 250))
    }
}

#[tokio::test]
async fn disk_usage_measures_graph_root_and_preserved_sizes() {
    let Harness {
        socket: _socket,
        docker,
        backend,
        cache: _cache,
        scratch: _scratch,
    } = harness().await;
    let seen = std::sync::Arc::default();
    let mut backend = backend.with_statvfs(FakeStatvfs(std::sync::Arc::clone(&seen)));
    let request = request(1, 2, 3);
    let sandbox = bounded(backend.create(&request)).await.unwrap();
    // Started then exited: the stopped container keeps its writable layer.
    bounded(backend.start_runner(&sandbox)).await.unwrap();
    docker.set_container_size(&request.name, 40);
    docker.set_volume_size(&format!("{}-work", request.name), 300);
    assert!(bounded(backend.stop(&sandbox)).await.is_confirmed());

    let none_preserved = bounded(backend.measured_disk_usage(&[])).await;
    assert_eq!(
        none_preserved.filesystem,
        Some(FsStats::from_blocks(4096, 1_000, 300, 250))
    );
    assert_eq!(none_preserved.preserved_bytes, Some(0));
    let usage = bounded(backend.measured_disk_usage(std::slice::from_ref(&sandbox))).await;
    assert_eq!(usage.preserved_bytes, Some(340), "SizeRw 40 + volume 300");
    assert_eq!(usage.retained_snapshot_bytes, None);
    assert_eq!(usage.saved_snapshots_over_allowance, None);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|p| p == std::path::Path::new("/proc"))
    );

    // An unmeasurable daemon leaves fields absent, never 0.
    for prefix in ["/v1.44/info", "/v1.44/system/df"] {
        docker.script(Rule {
            method: DockerMethod::Get,
            path_prefix: prefix.into(),
            effect: Effect::FailBefore(500),
        });
    }
    assert_eq!(
        bounded(backend.measured_disk_usage(&[sandbox])).await,
        BackendDiskUsage::default()
    );
}

struct FailedStatvfs;
impl Statvfs for FailedStatvfs {
    fn statvfs(&self, _: &std::path::Path) -> std::io::Result<FsStats> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }
}

#[tokio::test]
async fn measured_disk_usage_keeps_unknown_sizes_absent() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let sandbox = h.backend.create(&request).await.unwrap();
    let preserved = std::slice::from_ref(&sandbox);
    let seen = std::sync::Arc::default();
    let mut backend = h.backend.with_statvfs(FakeStatvfs(seen));
    let measure = |usage: BackendDiskUsage| {
        assert!(usage.filesystem.is_some());
        assert_eq!(usage.preserved_bytes, None);
    };
    // A present container without SizeRw is unknown, even if its volume is sized.
    h.docker
        .set_volume_size(&format!("{}-work", request.name), 300);
    measure(backend.measured_disk_usage(preserved).await);
    h.docker.set_container_size(&request.name, 40);
    // Docker uses -1 when it cannot measure a volume.
    h.docker
        .set_volume_size(&format!("{}-work", request.name), -1);
    measure(backend.measured_disk_usage(preserved).await);
    h.docker
        .set_volume_size(&format!("{}-work", request.name), 300);
    h.docker.script(Rule {
        method: DockerMethod::Get,
        path_prefix: format!("/v1.44/containers/{}/json", request.name),
        effect: Effect::FailBefore(500),
    });
    measure(backend.measured_disk_usage(preserved).await);
    // Filesystem and preserved measurements fail independently.
    let mut backend = backend.with_statvfs(FailedStatvfs);
    let usage = backend.measured_disk_usage(preserved).await;
    assert_eq!(usage.filesystem, None);
    assert_eq!(usage.preserved_bytes, Some(340));
}

#[tokio::test]
async fn measured_disk_usage_sizes_volume_only_preserved_sandboxes() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let sandbox = h.backend.create(&request).await.unwrap();
    h.docker
        .set_volume_size(&format!("{}-work", request.name), 300);
    // Stopping an unstarted sandbox removes the container and keeps its volume.
    assert!(h.backend.stop(&sandbox).await.is_confirmed());
    let mut backend = h.backend.with_statvfs(FailedStatvfs);
    let usage = backend
        .measured_disk_usage(std::slice::from_ref(&sandbox))
        .await;
    assert_eq!(usage.preserved_bytes, Some(300));
    let authorization = DestroyAuthorization::retained(sandbox.clone(), "retained").unwrap();
    assert!(
        backend
            .destroy(&sandbox, &authorization)
            .await
            .is_confirmed()
    );
    // Stale preserved identities cannot turn an unmeasurable size into zero.
    let usage = backend.measured_disk_usage(&[sandbox]).await;
    assert_eq!(usage.preserved_bytes, None);
}
