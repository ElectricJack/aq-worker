//! docker-local backend. Plain runs use a scripted Docker API double; tests
//! marked `#[ignore]` run against a real dedicated sandbox daemon whose socket
//! is given by `AQ_DOCKER_LOCAL_SOCKET`.
#[allow(dead_code)]
#[path = "support/docker_double.rs"]
mod docker_double;
#[allow(dead_code)]
#[path = "support/fault_transport.rs"]
mod fault_transport;

use aq_worker::backend::docker_local::*;
use aq_worker::backend::*;
use docker_double::*;
use fault_transport::FaultTransport;
use std::{os::unix::net::UnixListener, path::PathBuf, time::Duration};

const REFUSE_MAIN: BackendError =
    BackendError::InvalidRequest("refusing the host's main Docker daemon");

/// A Unix socket owned by the test user, so config validation passes while
/// requests go to the in-memory double.
struct OwnedSocket {
    _dir: tempfile::TempDir,
    _listener: UnixListener,
    path: PathBuf,
}
fn owned_socket() -> OwnedSocket {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sandbox.sock");
    let listener = UnixListener::bind(&path).unwrap();
    OwnedSocket {
        _dir: dir,
        _listener: listener,
        path,
    }
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test step hung")
}

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

// ---------------------------------------------------------------------------
// Launch policy, labelled inventory, delivery, stop and destroy (scripted).

use aq_worker::backend::conformance::{self, BackendFixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

const HEX: &str = "a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";
const LIMITS: ResourceLimits = ResourceLimits {
    cpu_millis: 500,
    memory_bytes: 64 * 1024 * 1024,
    pids: 32,
    disk_bytes: None,
};
const NOT_ALLOWLISTED: BackendError =
    BackendError::InvalidRequest("template digest not allowlisted");

fn digest() -> TemplateDigest {
    TemplateDigest::parse(format!("sha256:{HEX}")).unwrap()
}
fn image() -> String {
    format!("docker.io/library/busybox@sha256:{HEX}")
}

struct Harness {
    socket: OwnedSocket,
    docker: ScriptedDocker,
    backend: DockerLocalBackend<ScriptedDocker>,
    cache: tempfile::TempDir,
    scratch: tempfile::TempDir,
}

fn base_config(
    socket: &OwnedSocket,
    cache: &tempfile::TempDir,
    scratch: &tempfile::TempDir,
) -> DockerLocalConfig {
    let mut config = DockerLocalConfig::new(socket.path.clone());
    config.templates.push(AllowedTemplate {
        image: image(),
        digest: digest(),
    });
    config.runner_command = Some(vec!["/aq/bin/runner".into()]);
    config.mount_aliases.insert(
        "repo-cache".into(),
        MountAlias {
            host_path: cache.path().to_str().unwrap().into(),
            target: "/aq/cache".into(),
            read_only: true,
        },
    );
    config.mount_aliases.insert(
        "scratch".into(),
        MountAlias {
            host_path: scratch.path().to_str().unwrap().into(),
            target: "/aq/scratch".into(),
            read_only: false,
        },
    );
    config
}

async fn harness_with(
    docker: ScriptedDocker,
    edit: impl FnOnce(&mut DockerLocalConfig),
) -> Harness {
    let socket = owned_socket();
    let cache = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut config = base_config(&socket, &cache, &scratch);
    edit(&mut config);
    let backend = bounded(DockerLocalBackend::connect(config, docker.clone()))
        .await
        .unwrap();
    docker.clear_requests();
    Harness {
        socket,
        docker,
        backend,
        cache,
        scratch,
    }
}
async fn harness() -> Harness {
    harness_with(ScriptedDocker::new(), |_| {}).await
}

fn owner(node: u128, boot: u128, attempt: u128) -> Ownership {
    Ownership {
        node_id: Uuid::from_u128(node),
        boot_id: Uuid::from_u128(boot),
        attempt_id: Uuid::from_u128(attempt),
    }
}
fn request(node: u128, boot: u128, attempt: u128) -> CreateRequest {
    CreateRequest::new(
        owner(node, boot, attempt),
        digest(),
        3_600_000,
        vec![Capability::PersistentDiskDuringAttempt],
    )
    .with_resources(LIMITS)
}

fn posts(docker: &ScriptedDocker) -> Vec<String> {
    docker
        .requests()
        .into_iter()
        .filter(|(method, _)| *method != DockerMethod::Get)
        .map(|(_, path)| path)
        .collect()
}
fn deletes(docker: &ScriptedDocker) -> Vec<String> {
    docker
        .requests()
        .into_iter()
        .filter(|(method, _)| *method == DockerMethod::Delete)
        .map(|(_, path)| path)
        .collect()
}

fn expected_labels(request: &CreateRequest) -> Value {
    let hash: String = Sha256::digest(serde_json::to_vec(request).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({
        "aq.managed-by": "aq-worker",
        "aq.backend": "docker-local",
        "aq.node": request.owner.node_id.to_string(),
        "aq.boot": request.owner.boot_id.to_string(),
        "aq.attempt": request.owner.attempt_id.to_string(),
        "aq.template-digest": format!("sha256:{HEX}"),
        "aq.hard-deadline-ms": "3600000",
        "aq.request-sha256": hash,
    })
}

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

#[tokio::test]
async fn create_is_idempotent_for_identical_request() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let first = bounded(h.backend.create(&request)).await.unwrap();
    h.docker.clear_requests();
    let second = bounded(h.backend.create(&request)).await.unwrap();
    assert_eq!(first, second);
    assert!(posts(&h.docker).is_empty(), "{:?}", h.docker.requests());
    assert_eq!(h.docker.container_names(), vec![request.name.clone()]);
}

#[tokio::test]
async fn create_conflicts_on_foreign_same_name() {
    let mut h = harness().await;
    let original = request(1, 2, 3);
    bounded(h.backend.create(&original)).await.unwrap();
    let before = h.docker.container(&original.name).unwrap().body;
    h.docker.clear_requests();

    let mut different = original.clone();
    different.hard_deadline_ms += 1;
    assert_eq!(
        bounded(h.backend.create(&different)).await,
        Err(BackendError::Conflict)
    );
    h.docker.edit_container_labels(&original.name, |labels| {
        labels["aq.managed-by"] = json!("someone-else");
    });
    assert_eq!(
        bounded(h.backend.create(&original)).await,
        Err(BackendError::Conflict)
    );
    assert!(
        posts(&h.docker).is_empty(),
        "never adopt or delete a foreign object"
    );
    assert_eq!(
        h.docker.container(&original.name).unwrap().body["Image"],
        before["Image"]
    );

    // A foreign volume holding the work-volume name blocks create too.
    let other = request(1, 2, 4);
    let volume = format!("{}-work", other.name);
    let seeded = h
        .docker
        .send(DockerRequest {
            method: DockerMethod::Post,
            path: "/v1.44/volumes/create".into(),
            content_type: Some("application/json"),
            body: serde_json::to_vec(&json!({"Name": volume, "Labels": {"owner": "operator"}}))
                .unwrap()
                .into(),
            max_response_bytes: 1 << 20,
        })
        .await
        .unwrap();
    assert_eq!(seeded.status, 201);
    h.docker.clear_requests();
    assert_eq!(
        bounded(h.backend.create(&other)).await,
        Err(BackendError::Conflict)
    );
    assert!(h.docker.container(&other.name).is_none());
    assert_eq!(
        h.docker.volume(&volume).unwrap().labels,
        json!({"owner": "operator"})
    );
    assert!(deletes(&h.docker).is_empty());
}

#[tokio::test]
async fn lost_create_reply_is_discoverable() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: "/v1.44/containers/create".into(),
        effect: Effect::DropReplyAfterApply,
    });
    assert!(matches!(
        bounded(h.backend.create(&request)).await,
        Err(BackendError::Uncertain(_))
    ));
    let owned = bounded(h.backend.list_owned(request.owner.node_id))
        .await
        .unwrap();
    assert_eq!(owned.len(), 1);
    let entry = &owned[0];
    assert_eq!(entry.name, request.name);
    assert_eq!(entry.template_digest, request.template_digest);
    assert_eq!(entry.hard_deadline_ms, request.hard_deadline_ms);
    assert_eq!(entry.state, SandboxState::Created);
    assert!(entry.disk_present);
    assert_eq!(
        bounded(h.backend.create(&request)).await.unwrap(),
        entry.sandbox
    );
}

#[tokio::test]
async fn list_owned_scopes_node_and_includes_old_boots() {
    let mut h = harness().await;
    let old_boot = request(1, 1, 1);
    let current = request(1, 2, 2);
    let other_node = request(9, 2, 3);
    let volume_only = request(1, 2, 4);
    for request in [&old_boot, &current, &other_node, &volume_only] {
        bounded(h.backend.create(request)).await.unwrap();
    }
    let sandbox = bounded(h.backend.inspect(&SandboxIdentity {
        kind: BackendKind::DockerLocal,
        backend_id: volume_only.name.clone(),
        owner: volume_only.owner.clone(),
    }))
    .await
    .unwrap()
    .sandbox;
    // Stopping a never-started sandbox removes only its container.
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    assert!(h.docker.container(&volume_only.name).is_none());
    h.docker.clear_requests();

    let owned = bounded(h.backend.list_owned(Uuid::from_u128(1)))
        .await
        .unwrap();
    let mut names: Vec<&str> = owned.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    let mut expected = vec![
        old_boot.name.as_str(),
        current.name.as_str(),
        volume_only.name.as_str(),
    ];
    expected.sort();
    assert_eq!(names, expected);
    let entry = owned.iter().find(|e| e.name == volume_only.name).unwrap();
    assert_eq!(entry.state, SandboxState::Stopped);
    assert_eq!(entry.running_processes, Some(0));
    assert!(entry.disk_present);
    assert_eq!(entry.sandbox, sandbox);
    let old = owned.iter().find(|e| e.name == old_boot.name).unwrap();
    assert_eq!(old.sandbox.owner, old_boot.owner);
    let node = Uuid::from_u128(1).to_string();
    for (_, path) in h.docker.requests() {
        assert!(path.contains(&format!("aq.node%3D{node}")), "{path}");
    }
}

#[tokio::test]
async fn inventory_failure_is_err() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.api_timeout_ms = 100).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let node = request.owner.node_id;
    for prefix in ["/v1.44/containers/json", "/v1.44/volumes?"] {
        h.docker.script(Rule {
            method: DockerMethod::Get,
            path_prefix: prefix.into(),
            effect: Effect::FailBefore(500),
        });
        assert_eq!(
            bounded(h.backend.list_owned(node)).await,
            Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
            "{prefix}"
        );
    }
    h.docker.pause();
    assert_eq!(
        bounded(h.backend.list_owned(node)).await,
        Err(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    h.docker.resume();
    h.docker.edit_container_labels(&request.name, |labels| {
        labels.as_object_mut().unwrap().remove("aq.attempt");
    });
    let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
    assert_eq!(
        bounded(h.backend.list_owned(node)).await,
        Err(unknown.clone())
    );
    assert_eq!(bounded(h.backend.inspect(&sandbox)).await, Err(unknown));
}

// ---------------------------------------------------------------------------
// Material delivery, runner start, bounded reads, stop and destroy proofs.

#[tokio::test]
async fn material_is_delivered_by_archive_only() {
    let mut h = harness_with(ScriptedDocker::new(), |config| {
        config.material_uid = 1000;
        config.material_gid = 1001;
    })
    .await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let secret = b"top-secret-material-0123";
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(secret.to_vec())),
    )
    .await
    .unwrap();
    let container = h.docker.container(&request.name).unwrap();
    let file = &container.layer["/aq/private/material"];
    assert_eq!(file.data, secret);
    assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));
    assert!(h.docker.requests().contains(&(
        DockerMethod::Put,
        format!("/v1.44/containers/{}/archive?path=%2F", request.name)
    )));
    let needle = String::from_utf8_lossy(secret).to_string();
    assert!(
        !container.body.to_string().contains(&needle),
        "never env or argv"
    );
    for (_, path) in h.docker.requests() {
        assert!(!path.contains(&needle));
    }
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(
            h.backend
                .deliver_private_material(&sandbox, PrivateMaterial::new(secret.to_vec()))
        )
        .await,
        Err(BackendError::InvalidState),
        "material goes in before the runner starts"
    );
}

#[tokio::test]
async fn start_runner_and_bounded_read() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.max_read_bytes = 1024).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec())),
    )
    .await
    .unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/fixture-output"))
            .await
            .unwrap(),
        b"fixture-complete\n"
    );
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/missing")).await,
        Err(BackendError::NotFound)
    );
    let cap = Err(BackendError::InvalidRequest("archive exceeds read cap"));
    h.docker
        .put_file(&request.name, "/aq/work/big", vec![7; 1025]);
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/big")).await,
        cap
    );
    h.docker
        .put_file(&request.name, "/aq/work/huge", vec![7; 1024 * 1024]);
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/huge")).await,
        cap
    );
}

#[tokio::test]
async fn runner_exit_reports_the_observed_exit_code() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::ExitsWith(3)), |_| {}).await;
    let sandbox = bounded(h.backend.create(&request(1, 2, 3))).await.unwrap();
    assert_eq!(
        bounded(h.backend.runner_exit(&sandbox)).await,
        Err(BackendError::InvalidState),
        "a created, unstarted runner has no exit"
    );
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await.unwrap().state,
        SandboxState::Stopped
    );
    assert_eq!(
        bounded(h.backend.runner_exit(&sandbox)).await,
        Ok(RunnerExit::Exited(3))
    );
}

#[tokio::test]
async fn stop_is_proven_by_inspect() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::StaysRunning), |_| {}).await;
    let running = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&running)).await.unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await.unwrap().state,
        SandboxState::Running
    );
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: format!("/v1.44/containers/{}/stop", running.name),
        effect: Effect::LieOkWithoutApply,
    });
    assert_eq!(
        bounded(h.backend.stop(&sandbox)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::StillRunning),
        "a 204 is not proof; inspect is"
    );
    let proof = bounded(h.backend.stop(&sandbox)).await;
    assert!(
        matches!(&proof, Confirmation::Confirmed(p) if p.kind() == WriterProofKind::StoppedNoProcesses && p.sandbox() == &sandbox),
        "{proof:?}"
    );
    assert_eq!(h.docker.container(&running.name).unwrap().status, "exited");
    // Already exited: 304 is still confirmed by inspect.
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    let entry = bounded(h.backend.inspect(&sandbox)).await.unwrap();
    assert!(entry.disk_present);
    assert_eq!(
        reconcile(
            None,
            Some(&entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Preserve(sandbox.clone())
    );

    // Never started: stopped without being started; the volume stays.
    let idle = request(1, 2, 4);
    let idle_sandbox = bounded(h.backend.create(&idle)).await.unwrap();
    h.docker.clear_requests();
    assert!(bounded(h.backend.stop(&idle_sandbox)).await.is_confirmed());
    assert!(
        !h.docker
            .requests()
            .iter()
            .any(|(_, path)| path.ends_with("/start")),
    );
    assert!(h.docker.volume(&format!("{}-work", idle.name)).is_some());
}

#[tokio::test]
async fn destroy_requires_authorization_and_proves_absence() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let other = SandboxIdentity {
        kind: BackendKind::DockerLocal,
        backend_id: self::request(1, 2, 4).name,
        owner: owner(1, 2, 4),
    };
    let wrong = DestroyAuthorization::operator_released(other, "test").unwrap();
    h.docker.clear_requests();
    assert_eq!(
        bounded(h.backend.destroy(&sandbox, &wrong)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized)
    );
    assert!(h.docker.requests().is_empty());

    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.script(Rule {
        method: DockerMethod::Delete,
        path_prefix: "/v1.44/volumes/".into(),
        effect: Effect::LieOkWithoutApply,
    });
    assert!(
        matches!(
            bounded(h.backend.destroy(&sandbox, &release)).await,
            Confirmation::Unconfirmed(_)
        ),
        "a 204 DELETE with the volume still present is not destroyed"
    );
    assert!(h.docker.volume(&format!("{}-work", request.name)).is_some());

    let proof = bounded(h.backend.destroy(&sandbox, &release)).await;
    assert!(
        matches!(&proof, Confirmation::Confirmed(p) if p.kind() == WriterProofKind::Destroyed && p.sandbox() == &sandbox),
        "{proof:?}"
    );
    assert!(h.docker.container_names().is_empty());
    assert!(h.docker.volume_names().is_empty());
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await,
        Err(BackendError::NotFound)
    );
}

#[tokio::test]
async fn unreachable_daemon_is_unknown_never_gone() {
    let mut h = harness_with(
        ScriptedDocker::new().runner(Runner::StaysRunning),
        |config| {
            config.api_timeout_ms = 100;
        },
    )
    .await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.clear_requests();
    h.docker.pause();
    let timeout = BackendError::Uncertain(UnconfirmedReason::Timeout);
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await,
        Err(timeout.clone())
    );
    assert_eq!(
        bounded(h.backend.list_owned(request.owner.node_id)).await,
        Err(timeout)
    );
    let unconfirmed = Confirmation::Unconfirmed(UnconfirmedReason::Timeout);
    assert_eq!(bounded(h.backend.stop(&sandbox)).await, unconfirmed);
    assert_eq!(
        bounded(h.backend.destroy(&sandbox, &release)).await,
        unconfirmed
    );
    h.docker.resume();
    assert!(
        deletes(&h.docker).is_empty(),
        "no destroy while unreachable"
    );
    assert_eq!(h.docker.container(&request.name).unwrap().status, "running");
}

/// Conformance fixture over the scripted double: faults are injected at the
/// API boundary and the fixture output is read back through the archive API.
struct ScriptedFixture {
    harness: Harness,
    retained: BTreeMap<SandboxIdentity, Vec<u8>>,
}
impl BackendFixture for ScriptedFixture {
    type Backend = DockerLocalBackend<ScriptedDocker>;
    fn backend(&mut self) -> &mut Self::Backend {
        &mut self.harness.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        request(1, 2, case + 10)
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        let (method, path_prefix, effect) = match fault {
            Fault::CrashAfterCreate => (
                DockerMethod::Post,
                "/v1.44/containers/create",
                Effect::DropReplyAfterApply,
            ),
            Fault::DestroyFailure => (
                DockerMethod::Delete,
                "/v1.44/containers/",
                Effect::FailBefore(500),
            ),
            other => panic!("scripted fixture does not inject {other:?}"),
        };
        self.harness.docker.script(Rule {
            method,
            path_prefix: path_prefix.into(),
            effect,
        });
    }
    async fn observe_fixture(&mut self, sandbox: &SandboxIdentity) {
        let output = self
            .harness
            .backend
            .read_file(sandbox, "/aq/work/fixture-output")
            .await
            .unwrap();
        assert_eq!(output, b"fixture-complete\n");
    }
    async fn verify_and_retain(&mut self, sandbox: &SandboxIdentity) -> DestroyAuthorization {
        self.observe_fixture(sandbox).await;
        let output = self
            .harness
            .backend
            .read_file(sandbox, "/aq/work/fixture-output")
            .await
            .unwrap();
        self.retained.insert(sandbox.clone(), output);
        assert_eq!(self.retained[sandbox], b"fixture-complete\n");
        DestroyAuthorization::retained(
            sandbox.clone(),
            format!("scripted-retention/{}", sandbox.backend_id),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn scripted_conformance() {
    let mut fixture = ScriptedFixture {
        harness: harness().await,
        retained: BTreeMap::new(),
    };
    bounded(conformance::run_all(&mut fixture)).await;
    assert!(fixture.harness.docker.container_names().is_empty());
    assert!(fixture.harness.docker.volume_names().is_empty());
}

// ---------------------------------------------------------------------------
// WSL suspend/resume: a paused daemon is unknown, never gone.

#[tokio::test]
async fn wsl_suspend_resume_reconciles_sandboxes() {
    let mut h = harness_with(
        ScriptedDocker::new().runner(Runner::StaysRunning),
        |config| {
            config.api_timeout_ms = 100;
        },
    )
    .await;
    let request = request(1, 2, 3);
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec())),
    )
    .await
    .unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.clear_requests();

    h.docker.pause();
    suspended_is_unknown(&mut h.backend, &sandbox, &release).await;
    assert!(deletes(&h.docker).is_empty(), "no destroy during the pause");
    h.docker.resume();

    resumed_reconciles(&mut h.backend, &journal, &sandbox).await;
    assert!(
        h.docker
            .volume_names()
            .contains(&format!("{}-work", request.name))
    );
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    assert!(
        bounded(h.backend.destroy(&sandbox, &release))
            .await
            .is_confirmed()
    );
}

async fn suspended_is_unknown<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
    release: &DestroyAuthorization,
) {
    assert_eq!(
        bounded(backend.list_owned(sandbox.owner.node_id)).await,
        Err(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    assert!(bounded(backend.inspect(sandbox)).await.is_err());
    assert_eq!(
        bounded(backend.destroy(sandbox, release)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::Timeout)
    );
}

async fn resumed_reconciles<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    journal: &LifecycleRecord,
    sandbox: &SandboxIdentity,
) {
    let owned = bounded(backend.list_owned(sandbox.owner.node_id))
        .await
        .unwrap();
    let entry = owned
        .iter()
        .find(|entry| entry.sandbox == *sandbox)
        .expect("sandbox survives the suspend");
    assert!(entry.disk_present, "work volume survives the suspend");
    let action = reconcile(
        Some(journal),
        Some(entry),
        &RetentionState::Unretained,
        Deadline::Active,
    );
    assert!(
        matches!(&action, ReconcileAction::Adopt(s) | ReconcileAction::Preserve(s) if s == sandbox),
        "{action:?}"
    );
}

// ---------------------------------------------------------------------------
// Live qualification against the dedicated sandbox daemon (`--ignored`).

const LIVE_IMAGE: &str = "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for live docker-local tests"))
}

/// Live tests share one daemon and one of them pauses it, so they run one at
/// a time regardless of the harness's thread count.
static LIVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Live {
    _serial: tokio::sync::MutexGuard<'static, ()>,
    backend: DockerLocalBackend<FaultTransport<UnixSocketTransport>>,
    faults: FaultTransport<UnixSocketTransport>,
    node: Uuid,
    digest: TemplateDigest,
}

async fn live(runner: &[&str], edit: impl FnOnce(&mut DockerLocalConfig)) -> Live {
    let serial = LIVE_LOCK.lock().await;
    let socket = PathBuf::from(live_env("AQ_DOCKER_LOCAL_SOCKET"));
    let image = std::env::var("AQ_DOCKER_LOCAL_IMAGE").unwrap_or_else(|_| LIVE_IMAGE.into());
    let digest =
        TemplateDigest::parse(image.rsplit_once('@').expect("image pinned by digest").1).unwrap();
    let mut config = DockerLocalConfig::new(socket.clone());
    config.templates.push(AllowedTemplate {
        image,
        digest: digest.clone(),
    });
    config.runner_command = Some(runner.iter().map(|arg| arg.to_string()).collect());
    config.api_timeout_ms = 30_000;
    config.stop_grace_secs = 1;
    // SBX20: a dev daemon whose storage shares `/` must opt in explicitly.
    config.shared_root_fs = std::env::var_os("AQ_DOCKER_LOCAL_SHARED_ROOT_FS").is_some();
    edit(&mut config);
    let faults = FaultTransport::new(UnixSocketTransport::new(socket));
    let mut backend = DockerLocalBackend::connect(config, faults.clone())
        .await
        .expect("connect to the dedicated sandbox daemon");
    backend.pull_template(&digest).await.expect("pull template");
    Live {
        _serial: serial,
        backend,
        faults,
        node: Uuid::new_v4(),
        digest,
    }
}

impl Live {
    fn request(&self, attempt: u128) -> CreateRequest {
        CreateRequest::new(
            Ownership {
                node_id: self.node,
                boot_id: Uuid::from_u128(2),
                attempt_id: Uuid::from_u128(attempt),
            },
            self.digest.clone(),
            3_600_000,
            vec![Capability::PersistentDiskDuringAttempt],
        )
        .with_resources(LIMITS)
    }

    /// Best-effort cleanup scoped to this run's node label.
    async fn cleanup(&mut self) {
        let Ok(owned) = self.backend.list_owned(self.node).await else {
            return;
        };
        for entry in owned {
            let _ = self.backend.stop(&entry.sandbox).await;
            if let Ok(release) =
                DestroyAuthorization::operator_released(entry.sandbox.clone(), "live-cleanup")
            {
                let _ = self.backend.destroy(&entry.sandbox, &release).await;
            }
        }
    }

    async fn wait_for_file(&mut self, sandbox: &SandboxIdentity, path: &str) -> Vec<u8> {
        for _ in 0..100 {
            if let Ok(data) = self.backend.read_file(sandbox, path).await {
                return data;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{path} never appeared");
    }
}

const LIVE_RUNNER: &[&str] = &[
    "sh",
    "-c",
    "test -s /aq/private/material && printf 'fixture-complete\\n' > /aq/work/fixture-output",
];

struct LiveFixture {
    live: Live,
    retained: BTreeMap<SandboxIdentity, Vec<u8>>,
}
impl BackendFixture for LiveFixture {
    type Backend = DockerLocalBackend<FaultTransport<UnixSocketTransport>>;
    fn backend(&mut self) -> &mut Self::Backend {
        &mut self.live.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        self.live.request(case + 10)
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        let (method, path_prefix, effect) = match fault {
            Fault::CrashAfterCreate => (
                DockerMethod::Post,
                "/v1.44/containers/create",
                Effect::DropReplyAfterApply,
            ),
            Fault::DestroyFailure => (
                DockerMethod::Delete,
                "/v1.44/containers/",
                Effect::FailBefore(500),
            ),
            other => panic!("live fixture does not inject {other:?}"),
        };
        self.live.faults.script(Rule {
            method,
            path_prefix: path_prefix.into(),
            effect,
        });
    }
    async fn observe_fixture(&mut self, sandbox: &SandboxIdentity) {
        let output = self
            .live
            .wait_for_file(sandbox, "/aq/work/fixture-output")
            .await;
        assert_eq!(output, b"fixture-complete\n");
    }
    async fn verify_and_retain(&mut self, sandbox: &SandboxIdentity) -> DestroyAuthorization {
        let output = self
            .live
            .wait_for_file(sandbox, "/aq/work/fixture-output")
            .await;
        assert_eq!(output, b"fixture-complete\n");
        self.retained.insert(sandbox.clone(), output);
        DestroyAuthorization::retained(
            sandbox.clone(),
            format!("live-retention/{}", sandbox.backend_id),
        )
        .unwrap()
    }
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET"]
async fn live_conformance() {
    let mut fixture = LiveFixture {
        live: live(LIVE_RUNNER, |_| {}).await,
        retained: BTreeMap::new(),
    };
    conformance::run_all(&mut fixture).await;
    let leftover = fixture.live.backend.list_owned(fixture.live.node).await;
    fixture.live.cleanup().await;
    assert_eq!(leftover.map(|owned| owned.len()), Ok(0));
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET"]
async fn live_launch_policy_applied() {
    let probe = "grep -E '^(CapEff|NoNewPrivs):' /proc/self/status > /aq/work/policy; \
                 for f in cpu.max memory.max pids.max; do \
                 printf '%s: %s\\n' \"$f\" \"$(cat /sys/fs/cgroup/$f)\" >> /aq/work/policy; done";
    let mut live = live(&["sh", "-c", probe], |_| {}).await;
    let request = live.request(1);
    let sandbox = live.backend.create(&request).await.unwrap();
    live.backend.start_runner(&sandbox).await.unwrap();
    let policy = String::from_utf8(live.wait_for_file(&sandbox, "/aq/work/policy").await).unwrap();
    live.cleanup().await;
    let line = |key: &str| {
        policy
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .map(str::trim)
            .unwrap_or_else(|| panic!("{key} missing from probe output:\n{policy}"))
            .to_string()
    };
    assert_eq!(line("CapEff"), "0000000000000000", "{policy}");
    assert_eq!(line("NoNewPrivs"), "1", "{policy}");
    assert_eq!(line("cpu.max"), "50000 100000", "{policy}");
    assert_eq!(line("memory.max"), "67108864", "{policy}");
    assert_eq!(line("pids.max"), "32", "{policy}");
}

fn signal_daemon(signal: &str) {
    for pid in live_env("AQ_DOCKER_LOCAL_DAEMON_PID")
        .split([',', ' '])
        .filter(|p| !p.is_empty())
    {
        let status = std::process::Command::new("kill")
            .args([signal, pid])
            .status()
            .unwrap();
        assert!(status.success(), "kill {signal} {pid}");
    }
}

#[tokio::test]
#[ignore = "needs a dedicated sandbox daemon: AQ_DOCKER_LOCAL_SOCKET, AQ_DOCKER_LOCAL_DAEMON_PID"]
async fn live_wsl_suspend_resume_reconciles_sandboxes() {
    live_env("AQ_DOCKER_LOCAL_DAEMON_PID");
    let mut live = live(&["sh", "-c", "sleep 600"], |config| {
        config.api_timeout_ms = 3000
    })
    .await;
    let request = live.request(1);
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    let sandbox = live.backend.create(&request).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    live.backend
        .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec()))
        .await
        .unwrap();
    live.backend.start_runner(&sandbox).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    live.faults.clear_requests();

    signal_daemon("-STOP");
    let suspended = tokio::time::timeout(
        Duration::from_secs(30),
        suspended_is_unknown(&mut live.backend, &sandbox, &release),
    )
    .await;
    signal_daemon("-CONT");
    suspended.expect("suspended checks hung");
    assert!(
        !live
            .faults
            .requests()
            .iter()
            .any(|(method, _)| *method == DockerMethod::Delete),
        "no destroy during the pause"
    );
    resumed_reconciles(&mut live.backend, &journal, &sandbox).await;
    assert_eq!(
        live.backend.inspect(&sandbox).await.unwrap().state,
        SandboxState::Running,
        "the container keeps running across a daemon pause"
    );
    live.cleanup().await;
    assert_eq!(
        live.backend.list_owned(live.node).await.map(|o| o.len()),
        Ok(0)
    );
}

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
