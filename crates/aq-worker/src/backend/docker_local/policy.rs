//! Fixed launch policy and label identity. Nothing in an offer or request can
//! widen the create body: limits come from the reservation, mounts only from
//! the per-attempt volume, operator cache opt-in and declared aliases.
use super::config::{DockerLocalConfig, SCCACHE_CACHE_SIZE, SCCACHE_TARGET, WORK_TARGET};
use crate::backend::{
    BackendError, BackendKind, CreateRequest, InventoryEntry, Ownership, ResourceLimits,
    SandboxIdentity, SandboxState, TemplateDigest, UnconfirmedReason,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const LABEL_MANAGED_BY: &str = "aq.managed-by";
pub const MANAGED_BY: &str = "aq-worker";
pub const LABEL_BACKEND: &str = "aq.backend";
pub const BACKEND: &str = "docker-local";
pub const LABEL_NODE: &str = "aq.node";
pub const LABEL_BOOT: &str = "aq.boot";
pub const LABEL_ATTEMPT: &str = "aq.attempt";
pub const LABEL_DIGEST: &str = "aq.template-digest";
pub const LABEL_DEADLINE: &str = "aq.hard-deadline-ms";
pub const LABEL_REQUEST: &str = "aq.request-sha256";

/// Node-owned cache; unlike work volumes, never labelled with attempt ownership.
pub fn sccache_volume_name(node: Uuid) -> String {
    format!("aq-{}-sccache", node.simple())
}

pub fn volume_name(name: &str) -> String {
    format!("{name}-work")
}

/// Hex sha256 of the canonical request encoding; binds a name to one request.
pub fn request_hash(request: &CreateRequest) -> String {
    let encoded = serde_json::to_vec(request).expect("CreateRequest serializes");
    Sha256::digest(&encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn labels(request: &CreateRequest) -> BTreeMap<String, String> {
    let owner = &request.owner;
    BTreeMap::from([
        (LABEL_MANAGED_BY.into(), MANAGED_BY.into()),
        (LABEL_BACKEND.into(), BACKEND.into()),
        (LABEL_NODE.into(), owner.node_id.hyphenated().to_string()),
        (LABEL_BOOT.into(), owner.boot_id.hyphenated().to_string()),
        (
            LABEL_ATTEMPT.into(),
            owner.attempt_id.hyphenated().to_string(),
        ),
        (LABEL_DIGEST.into(), request.template_digest.as_str().into()),
        (LABEL_DEADLINE.into(), request.hard_deadline_ms.to_string()),
        (LABEL_REQUEST.into(), request_hash(request)),
    ])
}

/// Label filter selecting every sandbox this worker owns on `node`, any boot.
pub fn node_filter(node: Uuid) -> String {
    let filter = json!({
        "label": [
            format!("{LABEL_MANAGED_BY}={MANAGED_BY}"),
            format!("{LABEL_BACKEND}={BACKEND}"),
            format!("{LABEL_NODE}={}", node.hyphenated()),
        ]
    });
    encode(&filter.to_string())
}

/// The complete create body. Callers have already validated `request` against
/// the config: resources present, digest allowlisted, aliases declared.
pub fn create_body(
    config: &DockerLocalConfig,
    request: &CreateRequest,
    image: &str,
    resources: ResourceLimits,
) -> Value {
    let mut mounts = vec![json!({
        "Type": "volume",
        "Source": volume_name(&request.name),
        "Target": WORK_TARGET,
        "ReadOnly": false,
    })];
    let mut env = Vec::new();
    if config.shared_sccache {
        mounts.push(json!({
            "Type": "volume",
            "Source": sccache_volume_name(request.owner.node_id),
            "Target": SCCACHE_TARGET,
            "ReadOnly": false,
        }));
        env.push(format!("SCCACHE_DIR={SCCACHE_TARGET}"));
        env.push(format!("SCCACHE_CACHE_SIZE={SCCACHE_CACHE_SIZE}"));
    }
    for alias in &request.mount_aliases {
        let declared = &config.mount_aliases[alias];
        mounts.push(json!({
            "Type": "bind",
            "Source": declared.host_path,
            "Target": declared.target,
            "ReadOnly": declared.read_only,
        }));
    }
    let mut body = json!({
        "Image": image,
        "Labels": labels(request),
        "Env": env,
        "WorkingDir": WORK_TARGET,
        "ExposedPorts": {},
        "AttachStdin": false,
        "OpenStdin": false,
        "Tty": false,
        "HostConfig": {
            "Privileged": false,
            "CapAdd": [],
            "CapDrop": ["ALL"],
            "Devices": [],
            "DeviceCgroupRules": [],
            "SecurityOpt": ["no-new-privileges"],
            "NanoCpus": u64::from(resources.cpu_millis) * 1_000_000,
            "Memory": resources.memory_bytes,
            "MemorySwap": resources.memory_bytes,
            "PidsLimit": resources.pids,
            "NetworkMode": "bridge",
            "PortBindings": {},
            "PublishAllPorts": false,
            "Binds": [],
            "Mounts": mounts,
            "IpcMode": "private",
            "RestartPolicy": { "Name": "no" },
            "AutoRemove": false,
        },
    });
    if let Some(command) = &config.runner_command {
        body["Cmd"] = json!(command);
    }
    body
}

/// Maps a daemon container status to a backend state and process count.
pub fn container_state(status: &str, running: bool) -> (SandboxState, Option<u64>) {
    match (status, running) {
        ("created", false) => (SandboxState::Created, Some(0)),
        ("running", true) => (SandboxState::Running, None),
        ("exited" | "dead", false) => (SandboxState::Stopped, Some(0)),
        _ => (SandboxState::Unknown, None),
    }
}

/// Rebuilds an inventory entry from labels written at create. Missing,
/// malformed or foreign-managed labels are uncertainty, never absence.
pub fn entry_from_labels(
    name: &str,
    labels: &Value,
    state: SandboxState,
    running_processes: Option<u64>,
    disk_present: bool,
) -> Result<InventoryEntry, BackendError> {
    let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
    let label = |key: &str| {
        labels
            .get(key)
            .and_then(Value::as_str)
            .ok_or(unknown.clone())
    };
    let uuid = |key: &str| {
        label(key).and_then(|value| Uuid::parse_str(value).map_err(|_| unknown.clone()))
    };
    if label(LABEL_MANAGED_BY)? != MANAGED_BY || label(LABEL_BACKEND)? != BACKEND {
        return Err(unknown);
    }
    let owner = Ownership {
        node_id: uuid(LABEL_NODE)?,
        boot_id: uuid(LABEL_BOOT)?,
        attempt_id: uuid(LABEL_ATTEMPT)?,
    };
    let hash = label(LABEL_REQUEST)?;
    if name != owner.deterministic_name()
        || hash.len() != 64
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(unknown);
    }
    let template_digest =
        TemplateDigest::parse(label(LABEL_DIGEST)?).map_err(|_| unknown.clone())?;
    let hard_deadline_ms = label(LABEL_DEADLINE)?
        .parse()
        .map_err(|_| unknown.clone())?;
    Ok(InventoryEntry {
        sandbox: SandboxIdentity {
            kind: BackendKind::DockerLocal,
            backend_id: name.to_string(),
            owner,
        },
        name: name.to_string(),
        template_digest,
        hard_deadline_ms,
        state,
        running_processes,
        disk_present,
    })
}

/// True when `labels` carries every expected label with the expected value.
pub fn labels_match(labels: &Value, expected: &BTreeMap<String, String>) -> bool {
    expected
        .iter()
        .all(|(key, value)| labels.get(key).and_then(Value::as_str) == Some(value))
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
