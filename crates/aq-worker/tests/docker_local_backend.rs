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

#[path = "docker_local/sandbox_driver_units.rs"]
mod sandbox_driver_units;

#[path = "docker_local/backend_types.rs"]
mod backend_types;

// Keep the integration harness test names stable for callers that select cases with --exact.
include!("docker_local/admission.rs");
include!("docker_local/launch_policy.rs");
include!("docker_local/inventory.rs");
include!("docker_local/lifecycle.rs");
include!("docker_local/live.rs");
include!("docker_local/disk.rs");
include!("docker_local/driver.rs");
include!("docker_local/driver_cases.rs");
include!("docker_local/observation.rs");
include!("docker_local/login.rs");
