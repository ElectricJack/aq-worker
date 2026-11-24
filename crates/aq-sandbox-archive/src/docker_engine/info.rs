//! Capability and filesystem checks on the Engine information response.
use serde_json::Value;
use std::path::Path;

pub fn daemon_is_rootless(info: &Value) -> bool {
    info.get("SecurityOptions")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options
                .iter()
                .filter_map(Value::as_str)
                .any(|option| option.split(',').any(|part| part == "name=rootless"))
        })
}

pub fn graph_root(info: &Value) -> Option<&Path> {
    info.get("DockerRootDir")
        .or_else(|| info.pointer("/Store/GraphRoot"))
        .or_else(|| info.pointer("/store/graphRoot"))
        .and_then(Value::as_str)
        .map(Path::new)
        .filter(|path| path.is_absolute())
}

/// SBX10: per-sandbox CPU, memory and PID limits must be enforced. A daemon
/// without a cgroup driver (e.g. `docker:dind-rootless`) or on cgroup v1 (no
/// rootless delegation) accepts `NanoCpus`/`Memory`/`PidsLimit` and silently
/// drops them, so it is refused rather than launching unlimited sandboxes. A
/// missing field is treated as unenforced.
pub fn require_enforced_limits(info: &Value) -> Result<(), &'static str> {
    let field = |name: &str| info.get(name).and_then(Value::as_str);
    match field("CgroupDriver") {
        None | Some("") | Some("none") => {
            return Err(
                "sandbox daemon reports no cgroup driver; resource limits would be silently dropped",
            );
        }
        Some(_) => {}
    }
    if field("CgroupVersion") != Some("2") {
        return Err("sandbox daemon must use cgroup v2; resource limits would be silently dropped");
    }
    Ok(())
}

/// SBX20 admission: the daemon's storage root (graphroot) must be on a
/// filesystem distinct from `/`, compared by `st_dev`. A missing or
/// unreadable root cannot be proven distinct and counts as shared.
pub fn storage_shares_root(info: &Value) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(root) = graph_root(info) else {
        return true;
    };
    match (std::fs::metadata(root), std::fs::metadata("/")) {
        (Ok(storage), Ok(host)) => storage.dev() == host.dev(),
        _ => true,
    }
}

/// `DriverStatus` is a list of `[key, value]` pairs.
pub fn backing_filesystem_is_xfs(info: &Value) -> bool {
    info["DriverStatus"].as_array().is_some_and(|pairs| {
        pairs.iter().any(|pair| {
            pair[0].as_str() == Some("Backing Filesystem") && pair[1].as_str() == Some("xfs")
        })
    })
}
