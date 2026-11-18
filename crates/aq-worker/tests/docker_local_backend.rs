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

/// A ChatGPT login document built at run time (no credential-shaped literal).
fn codex_login() -> (Vec<u8>, String) {
    let token = |name: &str| format!("fixture-{name}-{}", "z".repeat(24));
    let refresh = token("refresh");
    let auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": token("id"), "access_token": token("access"),
            "refresh_token": refresh, "account_id": "fixture-account"},
    });
    (serde_json::to_vec(&auth).unwrap(), refresh)
}

struct CodexDriverRun {
    /// What the pane showed, and what reached the sandbox terminal's input.
    screen: Vec<u8>,
    typed: Vec<u8>,
    attach: String,
    code: i32,
    docker: ScriptedDocker,
    dir: tempfile::TempDir,
    auth: Vec<u8>,
}
/// Runs the node driver's Codex launch (harness `codex`, the docker-local Codex
/// recipe) against the scripted double: the TUI stays running, its `notify`
/// program has left `final_message` in the stop marker, the sandbox terminal
/// prints `tui` and the owner types `keys`. `extra` are files the runner writes.
async fn run_codex_driver(final_message: &str, extra: &[(&str, Vec<u8>)]) -> CodexDriverRun {
    run_codex_driver_on(final_message, extra, b"codex tui\r\n", b"").await
}
async fn run_codex_driver_on(
    final_message: &str,
    extra: &[(&str, Vec<u8>)],
    tui: &[u8],
    keys: &[u8],
) -> CodexDriverRun {
    use aq_session_protocol::{Capability, HarnessSettings, ModelSettings, Text};
    use aq_worker::sandbox_driver::{DriverConfig, DriverHarness, run_with};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let socket = owned_socket();
    let (cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docker_local = base_config(&socket, &cache, &scratch);
    let docker = ScriptedDocker::new().runner(Runner::StaysRunning);
    let marker = json!({"type":"agent-turn-complete","last-assistant-message":final_message});
    docker.on_start_write("/aq/out/stop.json", marker.to_string().into_bytes());
    for (path, data) in extra {
        docker.on_start_write(path, data.clone());
    }

    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "session", "private"] {
        std::fs::create_dir(dir.path().join(d)).unwrap();
        std::fs::set_permissions(dir.path().join(d), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "# scratch\n").unwrap();
    std::fs::write(
        dir.path().join("session/context-0.bin"),
        "Write the fixture.",
    )
    .unwrap();
    let private = |path: &std::path::Path, data: &[u8]| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut f, data).unwrap();
    };
    let (auth, _) = codex_login();
    let token_file = dir.path().join("private/token");
    private(&token_file, &auth);

    let artifact = |id: &str| aq_session_protocol::ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: aq_session_protocol::Digest::of(id.as_bytes()),
    };
    let harness = HarnessSettings {
        harness: Text::new("codex").unwrap(),
        cli_version: Text::new(aq_codex_mapper::CODEX_CLI_VERSION).unwrap(),
        adapter: artifact("aq.codex-docker-result-local-adapter"),
        environment: artifact("environment"),
        required_capabilities: vec![Capability {
            id: Text::new(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
            version: 1,
        }],
    };
    let model = ModelSettings {
        provider: Text::new("openai").unwrap(),
        model: Text::new("gpt-5.5").unwrap(),
        reasoning: Text::new("high").unwrap(),
        max_output_tokens: 4096,
        spending_budget: None,
    };
    let recipe = aq_codex_mapper::docker_local::map_docker_local(
        aq_codex_mapper::Settings {
            harness: &harness,
            model: &model,
            terminal_input_enabled: false,
        },
        aq_codex_mapper::Paths {
            executable: std::path::Path::new("/opt/aq/bin/codex"),
            workspace: std::path::Path::new("/aq/work"),
            private_home: std::path::Path::new("/aq/home/codex"),
        },
        None,
    )
    .unwrap();
    let config = DriverConfig {
        docker_local,
        template: digest(),
        owner: owner(1, 2, 3),
        hard_deadline_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 600_000,
        resources: LIMITS,
        workspace: work,
        token_file: token_file.clone(),
        executable: recipe.executable.to_str().unwrap().into(),
        arguments: recipe.arguments,
        environment: recipe.environment,
        settings_json: String::new(),
        result_dir: None,
        max_workspace_bytes: 1 << 20,
        agent_channel: false,
        conversation_outbox: None,
        harness: DriverHarness::Codex,
        codex_runtime_bundle: None,
    };
    let config_file = dir.path().join("private/driver.json");
    private(&config_file, &serde_json::to_vec(&config).unwrap());
    let session = dir.path().join("session");
    let connect = |c| DockerLocalBackend::connect(c, docker.clone());
    // The sandbox terminal (a duplex stream) and the pane (two pipes).
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ours, mut sandbox) = tokio::io::duplex(1 << 16);
    let (input, mut keyboard) = std::io::pipe().unwrap();
    let (mut shown, output) = std::io::pipe().unwrap();
    keyboard.write_all(keys).unwrap();
    drop(keyboard);
    sandbox.write_all(tui).await.unwrap();
    let attached = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen = attached.clone();
    let terminal = |_socket: std::path::PathBuf, path: String| async move {
        *seen.lock().unwrap() = path;
        let pane = aq_worker::sandbox_driver::Pane {
            input: Box::new(input),
            output: Box::new(output),
            tty: false,
        };
        Ok((ours, pane))
    };
    let code = tokio::time::timeout(
        Duration::from_secs(60),
        run_with(&config_file, Some(&session), connect, terminal),
    )
    .await
    .expect("driver hung")
    .unwrap();
    assert!(!token_file.exists(), "the leased login never stays on disk");
    let mut typed = vec![0u8; keys.len()];
    sandbox.read_exact(&mut typed).await.unwrap();
    let mut screen = Vec::new();
    shown.read_to_end(&mut screen).unwrap();
    let attach = attached.lock().unwrap().clone();
    CodexDriverRun {
        screen,
        typed,
        attach,
        code,
        docker,
        dir,
        auth,
    }
}

/// Epic 0 W1: the Codex launch path in the docker-local sandbox, against the Docker
/// double: the same sandbox, copy-in/copy-out and stop as Claude; the Codex runner and
/// the interactive `codex --sandbox danger-full-access` on the sandbox's terminal with
/// the offered model and effort; the ChatGPT login only as private material; the
/// attempt ends when the turn does, with the claim from Codex's final message.
#[tokio::test]
async fn sandbox_driver_runs_codex_in_the_docker_local_sandbox() {
    let run = run_codex_driver(
        "Done.\nChanges: fixture-output; Checks: none; Limits: none",
        &[],
    )
    .await;
    assert_eq!(run.code, 0, "proven success");
    let work = run.dir.path().join("work");
    assert_eq!(
        std::fs::read(work.join("fixture-output")).unwrap(),
        b"fixture-complete\n",
        "the runner's work was copied back"
    );
    let names = run.docker.container_names();
    assert_eq!(names.len(), 1);
    let container = run.docker.container(&names[0]).unwrap();
    assert_eq!(container.status, "exited", "the sandbox is stopped");
    let body = container.body.to_string();
    assert!(body.contains("CODEX_HOME/auth.json") && !body.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    let text = |path: &str| String::from_utf8(container.layer[path].data.clone()).unwrap();
    let args: Vec<String> = text("/aq/in/args").lines().map(Into::into).collect();
    let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(args[0], "--no-daemon");
    assert!(
        !args.contains(&"exec".to_string()),
        "the TUI, not `codex exec`"
    );
    assert_eq!(args.last().map(String::as_str), Some("--"));
    assert_eq!(at("--sandbox"), "danger-full-access");
    assert_eq!(at("-m"), "gpt-5.5");
    // A terminal for the TUI, attached to the pane; the TUI never exits by itself.
    let create: Value = container.body.clone();
    for key in ["Tty", "OpenStdin", "AttachStdin"] {
        assert_eq!(create[key], true, "{key}");
    }
    assert!(
        run.attach.ends_with(&format!(
            "/containers/{}/attach?stream=1&stdin=1&stdout=1&stderr=1",
            names[0]
        )),
        "{}",
        run.attach
    );
    assert_eq!(run.screen, b"codex tui\r\n");
    assert!(args.contains(&"model_reasoning_effort=\"high\"".to_string()));
    assert_eq!(text("/aq/in/executable"), "/opt/aq/bin/codex");
    assert_eq!(
        text("/aq/in/env"),
        "CODEX_HOME=/aq/home/codex\nHOME=/aq/home/codex\n"
    );
    // The login reaches the sandbox only as private material, never argv, env or a path.
    assert_eq!(container.layer["/aq/private/material"].data, run.auth);
    let (_, refresh) = codex_login();
    assert!(!body.contains(&refresh));
    for (_, path) in run.docker.requests() {
        assert!(!path.contains(&refresh));
    }
    let private = run.dir.path().join("private");
    let exit: Value =
        serde_json::from_slice(&std::fs::read(private.join("exit.json")).unwrap()).unwrap();
    assert_eq!(exit["success"], true);
    assert!(private.join("codex.jsonl").exists() && !private.join("claude.jsonl").exists());
}

/// A login token that the agent writes into the workspace is refused: nothing is
/// copied back and the attempt is not a success.
#[tokio::test]
async fn sandbox_driver_refuses_a_codex_login_token_in_the_workspace() {
    let (_, refresh) = codex_login();
    let leak = format!("export T={refresh}\n").into_bytes();
    let run = run_codex_driver(
        "Done.\nChanges: leak.txt; Checks: none; Limits: none",
        &[("/aq/work/leak.txt", leak)],
    )
    .await;
    assert_ne!(run.code, 0);
    let work = run.dir.path().join("work");
    assert!(!work.join("leak.txt").exists() && !work.join("fixture-output").exists());
    let private = run.dir.path().join("private");
    assert!(
        !private.join("codex.jsonl").exists(),
        "no transcript when a secret leaked"
    );
}

/// A Claude driver configuration keeps its bytes (no `harness` key); a Codex one names
/// its harness and reads back the same.
#[test]
fn claude_driver_config_has_no_harness_key_and_codex_round_trips() {
    use aq_worker::sandbox_driver::{DriverConfig, DriverHarness};
    let claude = json!({
        "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
        "template": format!("sha256:{HEX}"),
        "owner": {"node_id": Uuid::from_u128(1), "boot_id": Uuid::from_u128(2),
            "attempt_id": Uuid::from_u128(3)},
        "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
        "workspace": "/w", "token_file": "/t", "executable": "/opt/aq/bin/claude", "arguments": ["-p"],
        "environment": {}, "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1,
    });
    let config: DriverConfig = serde_json::from_value(claude).unwrap();
    assert_eq!(config.harness, DriverHarness::Claude);
    let written = serde_json::to_value(&config).unwrap();
    assert!(written.get("harness").is_none(), "{written}");
    let mut codex = config;
    codex.harness = DriverHarness::Codex;
    let written = serde_json::to_value(&codex).unwrap();
    assert_eq!(written["harness"], "codex");
    let back: DriverConfig = serde_json::from_value(written).unwrap();
    assert_eq!(back.harness, DriverHarness::Codex);
}

/// What the owner types on the pane reaches the sandbox terminal as typed. A login
/// token the TUI prints is masked before it reaches the pane, and the attempt then
/// fails with nothing copied back and no transcript.
#[tokio::test]
async fn sandbox_driver_relays_typing_and_masks_a_login_token_on_the_pane() {
    let run = run_codex_driver_on(
        "Done.\nChanges: none; Checks: none; Limits: none",
        &[],
        b"ready\r\n",
        b"also update the README\r",
    )
    .await;
    assert_eq!(run.code, 0);
    assert_eq!(run.typed, b"also update the README\r");
    assert_eq!(run.screen, b"ready\r\n");

    let (_, refresh) = codex_login();
    let tui = format!("token: {refresh}\r\n");
    let run = run_codex_driver_on(
        "Done.\nChanges: none; Checks: none; Limits: none",
        &[],
        tui.as_bytes(),
        b"",
    )
    .await;
    assert_ne!(
        run.code, 0,
        "a login secret on the terminal fails the attempt"
    );
    let screen = String::from_utf8(run.screen).unwrap();
    assert_eq!(screen, format!("token: {}\r\n", "*".repeat(refresh.len())));
    let work = run.dir.path().join("work");
    assert!(!work.join("fixture-output").exists());
    assert!(!run.dir.path().join("private/codex.jsonl").exists());
}

/// The three tokens of a ChatGPT login are the secrets the driver refuses to let out.
#[test]
fn codex_login_tokens_are_the_three_login_secrets() {
    use aq_handoff_note::{Secrets, harness::codex_login_tokens};
    let (auth, refresh) = codex_login();
    let tokens = codex_login_tokens(&auth).unwrap();
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[2].as_slice(), refresh.as_bytes());
    let by = Secrets::of_tokens(tokens.iter().map(|t| t.as_slice()).collect(), None);
    for token in &tokens {
        assert!(by.holds(&[b"leak ".as_slice(), token].concat()));
    }
    assert!(!by.holds(b"fixture-refresh"));
    let api = json!({"OPENAI_API_KEY": "fixture-key-0123456789"}).to_string();
    assert!(codex_login_tokens(api.as_bytes()).is_err());
    assert!(codex_login_tokens(b"not json").is_err());
}

/// Unit checks of the node driver's public pieces (moved here from the library to
/// keep it within its line budget).
mod sandbox_driver_units {
    use aq_handoff_note::Secrets;
    use aq_worker::backend::docker_local::{DockerLocalConfig, tar::TreeEntry};
    use aq_worker::sandbox_driver::{
        CHANNEL_NOTE, DriverConfig, channel_ready, input_entries, run, token_leak,
    };
    use std::io::Write;

    /// MS1 review S3a: the token file is read and removed before anything that
    /// can fail, even when the driver config itself is unreadable.
    #[tokio::test]
    async fn token_file_is_removed_even_when_the_driver_config_is_bad() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("driver.json");
        let token = dir.path().join("token");
        for (path, data) in [
            (&config, &b"{not a config"[..]),
            (&token, b"fixture-login-0123456789"),
        ] {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .write_all(data)
                .unwrap();
        }
        assert!(run(&config).await.is_err());
        assert!(!token.exists(), "the leased token must not stay on disk");
    }

    /// 0006 §5.4/§8: no channel socket, no launch; the env names the socket;
    /// an `aqa_` credential anywhere that leaves the sandbox is a leak.
    #[test]
    fn channel_is_required_named_in_env_and_its_credential_guarded() {
        let root = tempfile::tempdir().unwrap();
        let mut config: DriverConfig = serde_json::from_value(serde_json::json!({
            "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
            "template": format!("sha256:{}", "a".repeat(64)),
            "owner": {"node_id": uuid::Uuid::from_u128(1), "boot_id": uuid::Uuid::from_u128(2),
                "attempt_id": uuid::Uuid::from_u128(3)},
            "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
            "workspace": "/w", "token_file": "/t", "executable": "/e", "arguments": [], "environment": {},
            "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1, "agent_channel": true,
        }))
        .unwrap();
        let error = channel_ready(&config).unwrap_err().to_string();
        assert!(error.starts_with("agent_channel_unavailable"), "{error}");
        config.docker_local.channel_root = Some(root.path().into());
        let dir = root.path().join(uuid::Uuid::from_u128(3).to_string());
        std::fs::create_dir(&dir).unwrap();
        assert!(
            channel_ready(&config).is_err(),
            "a directory without a socket"
        );
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("aq.sock")).unwrap();
        channel_ready(&config).unwrap();
        config.environment = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let inputs = input_entries(&config, b"p").unwrap();
        let text = |name: &str| match inputs.iter().find(|e| e.path() == name) {
            Some(TreeEntry::File { data, .. }) => String::from_utf8(data.clone()).unwrap(),
            _ => panic!("{name}"),
        };
        let env = "PATH=/opt/aq/bin:/usr/bin:/bin\nAQ_CHANNEL=unix:/aq/channel/aq.sock\n";
        assert_eq!(text("env"), env);

        assert!(text("prompt").ends_with(CHANNEL_NOTE));
        let credential = format!("aqa_{}", "Q".repeat(43));
        let file = |data: &str| TreeEntry::File {
            path: "f".into(),
            mode: 0o644,
            data: data.into(),
        };
        let token = &Secrets::new(b"fixture-login-0123456789abcdef", None);
        assert!(token_leak(&[file(&credential)], b"", token).is_some());
        assert!(token_leak(&[], credential.as_bytes(), token).is_some());
        assert!(token_leak(&[file("starts_with(\"aqa_\")")], b"", token).is_none());
        // A shape the base tree already held (a fixture in the repository) is no leak.
        let kept = &Secrets::new(b"fixture-login-0123456789abcdef", [credential.as_bytes()]);
        assert!(token_leak(&[file(&credential)], credential.as_bytes(), kept).is_none());
        assert!(token_leak(&[file(&format!("{credential}x"))], b"", kept).is_some());
        // A variable that would replace the subscription login is refused.
        config.environment = [("ANTHROPIC_API_KEY".into(), "x".into())].into();
        assert!(input_entries(&config, b"p").is_err(), "API key env refused");
    }
}

/// C5b for the docker-local sandbox: a Codex login that the stopped sandbox holds
/// refreshed (same account, later `last_refresh`) is written back to the server as
/// the next version of that attempt's lease; an unchanged or foreign login, another
/// attempt's sandbox, and an attempt without a lease file write nothing.
#[tokio::test]
async fn refreshed_codex_login_is_written_back_from_the_stopped_sandbox() {
    use aq_agent_channel::login::{LEASE_FILE, LoginLease, WriteBack};
    use aq_worker::native::runner::write_back_login;
    use axum::{Router, body::Bytes, routing::post};
    use std::os::unix::fs::OpenOptionsExt;
    let login = |account: &str, refreshed: &str| {
        json!({"auth_mode": "chatgpt", "last_refresh": refreshed,
            "tokens": {"id_token": "fixture-id", "access_token": "fixture-access",
                "refresh_token": "fixture-refresh", "account_id": account}})
        .to_string()
        .into_bytes()
    };
    let leased = login("fixture-account", "2026-11-12T00:00:00Z");
    let refreshed = login("fixture-account", "2026-11-18T00:00:00Z");

    let uploads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let seen = uploads.clone();
    let app = Router::new().route(
        "/v1/worker/profiled/material/refresh",
        post(move |body: Bytes| async move {
            seen.lock().unwrap().push(body.to_vec());
            r#"{"accepted":true,"version":4,"replayed":false}"#
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let link =
        aq_agent_channel::NodeLink::new(&origin, "node-credential-0123456789", None, None).unwrap();

    let driver = tempfile::tempdir().unwrap();
    let lease = LoginLease::of(&leased, json!({}), json!({}), Uuid::from_u128(9), 3).unwrap();
    let write_lease = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(driver.path().join(LEASE_FILE))
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, &serde_json::to_vec(&lease).unwrap())
            })
            .unwrap()
    };
    // One sandbox per case: what the stopped sandbox's auth.json holds.
    let cases = [
        (3, refreshed.clone(), Some(WriteBack::Accepted(4))),
        (4, leased.clone(), None),
        (5, login("another-account", "2026-11-18T00:00:00Z"), None),
        (6, b"{not a login".to_vec(), None),
    ];
    for (attempt, held, expected) in cases {
        let docker = ScriptedDocker::new().runner(Runner::StaysRunning);
        docker.on_start_write("/aq/home/codex/auth.json", held);
        let mut h = harness_with(docker, |_| {}).await;
        let request = self::request(1, 2, attempt);
        let sandbox = bounded(h.backend.create(&request)).await.unwrap();
        let material = PrivateMaterial::new(b"fixture".to_vec());
        bounded(h.backend.deliver_private_material(&sandbox, material))
            .await
            .unwrap();
        bounded(h.backend.start_runner(&sandbox)).await.unwrap();
        assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
        let ids = (Uuid::from_u128(1), Uuid::from_u128(attempt));
        // No lease file (a Claude attempt): nothing is read or sent.
        let none = write_back_login(&mut h.backend, ids, driver.path(), &link).await;
        assert_eq!(none.unwrap(), None);
        write_lease();
        let other = (ids.0, Uuid::from_u128(99));
        let foreign = write_back_login(&mut h.backend, other, driver.path(), &link).await;
        assert_eq!(
            foreign.unwrap(),
            None,
            "another attempt's sandbox is not read"
        );
        let written = write_back_login(&mut h.backend, ids, driver.path(), &link).await;
        assert_eq!(written.unwrap(), expected, "attempt {attempt}");
        std::fs::remove_file(driver.path().join(LEASE_FILE)).unwrap();
    }
    assert_eq!(
        *uploads.lock().unwrap(),
        vec![refreshed],
        "only the refreshed login left the node"
    );
}
