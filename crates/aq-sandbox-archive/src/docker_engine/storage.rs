use super::{
    CallError, DockerMethod, DockerRequest, DockerTransport, call, daemon_is_rootless, graph_root,
    require_enforced_limits,
};
use serde_json::Value;
use std::time::Duration;
use std::{ffi::CString, io, mem::MaybeUninit, os::unix::ffi::OsStrExt, path::Path};

/// One filesystem's capacity as statvfs reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    /// Space available to unprivileged writers (`f_bavail`).
    pub free_bytes: u64,
}
impl FsStats {
    /// `used` counts all allocated blocks (`blocks - bfree`), so `used + free`
    /// can be below `total` by the root reserve.
    pub fn from_blocks(fragment: u64, blocks: u64, bfree: u64, bavail: u64) -> Self {
        Self {
            total_bytes: blocks.saturating_mul(fragment),
            used_bytes: blocks.saturating_sub(bfree).saturating_mul(fragment),
            free_bytes: bavail.saturating_mul(fragment),
        }
    }
}

/// statvfs seam so tests measure a fake filesystem.
pub trait Statvfs: Send + Sync {
    fn statvfs(&self, path: &Path) -> io::Result<FsStats>;
}
pub struct HostStatvfs;
impl Statvfs for HostStatvfs {
    fn statvfs(&self, path: &Path) -> io::Result<FsStats> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut raw = MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid NUL-terminated path; statvfs fully initializes `raw` on 0.
        if unsafe { libc::statvfs(path.as_ptr(), raw.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = unsafe { raw.assume_init() };
        let fragment = if raw.f_frsize > 0 {
            raw.f_frsize
        } else {
            raw.f_bsize
        };
        #[allow(clippy::useless_conversion)]
        Ok(FsStats::from_blocks(
            u64::from(fragment),
            u64::from(raw.f_blocks),
            u64::from(raw.f_bfree),
            u64::from(raw.f_bavail),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlacementRefusal {
    #[error("sandbox_daemon_info_timeout")]
    InfoTimeout,
    #[error("sandbox_daemon_unavailable")]
    DaemonUnavailable,
    #[error("sandbox_daemon_unqualified")]
    DaemonUnqualified,
    #[error("sandbox_storage_unmeasurable")]
    StorageUnmeasurable,
    #[error("sandbox_disk_headroom_unavailable: {available} available, {minimum} required")]
    DiskHeadroom { available: u64, minimum: u64 },
}

/// Read the actual daemon's storage metadata. Check both overlay storage and
/// local work volumes when a volume directory is mounted separately.
pub async fn check_placement(
    transport: &impl DockerTransport,
    api_version: &str,
    timeout: Duration,
    minimum: u64,
    statvfs: &dyn Statvfs,
) -> Result<(), PlacementRefusal> {
    use PlacementRefusal as Refusal;
    let started = tokio::time::Instant::now();
    let info = daemon_info(transport, format!("{api_version}/info"), timeout).await?;
    if !daemon_is_rootless(&info) || require_enforced_limits(&info).is_err() {
        return Err(Refusal::DaemonUnqualified);
    }
    // Podman's Docker-compatible endpoint reports graphroot but omits volume_path.
    let info = if info.get("BuildahVersion").is_some() {
        let podman = daemon_info(
            transport,
            "/v4.0.0/libpod/info".into(),
            timeout.saturating_sub(started.elapsed()),
        )
        .await?;
        if volume_path(&podman).is_none() || graph_root(&podman) != graph_root(&info) {
            return Err(Refusal::StorageUnmeasurable);
        }
        podman
    } else {
        info
    };
    let available = storage_headroom(&info, statvfs)?;
    if available < minimum {
        return Err(Refusal::DiskHeadroom { available, minimum });
    }
    Ok(())
}

async fn daemon_info(
    transport: &impl DockerTransport,
    path: String,
    timeout: Duration,
) -> Result<Value, PlacementRefusal> {
    use PlacementRefusal as Refusal;
    let response = call(
        transport,
        DockerRequest {
            method: DockerMethod::Get,
            path,
            content_type: None,
            body: Default::default(),
            max_response_bytes: 8 * 1024 * 1024,
        },
        timeout,
    )
    .await
    .map_err(|error| match error {
        CallError::Timeout => Refusal::InfoTimeout,
        _ => Refusal::DaemonUnavailable,
    })?;
    if response.status != 200 {
        return Err(Refusal::DaemonUnavailable);
    }
    serde_json::from_slice(&response.body).map_err(|_| Refusal::DaemonUnavailable)
}

fn volume_path(info: &Value) -> Option<&Value> {
    info.pointer("/Store/VolumePath")
        .or_else(|| info.pointer("/store/volumePath"))
}

fn storage_headroom(info: &Value, statvfs: &dyn Statvfs) -> Result<u64, PlacementRefusal> {
    let unknown = PlacementRefusal::StorageUnmeasurable;
    let graph = graph_root(info).ok_or(unknown)?;
    let free = |path| {
        statvfs
            .statvfs(path)
            .map(|fs| fs.free_bytes)
            .map_err(|_| unknown)
    };
    let available = free(graph)?;
    let default_volumes = graph.join("volumes");
    let volumes = match volume_path(info) {
        Some(path) => Some(
            path.as_str()
                .map(Path::new)
                .filter(|p| p.is_absolute())
                .ok_or(unknown)?,
        ),
        None => match std::fs::metadata(&default_volumes) {
            Ok(directory) if directory.is_dir() => Some(default_volumes.as_path()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            _ => return Err(unknown),
        },
    };
    match volumes {
        Some(volumes) if volumes != graph => Ok(available.min(free(volumes)?)),
        _ => Ok(available),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_engine::{DockerResponse, TransportError};
    use serde_json::json;
    use std::sync::Mutex;

    struct SeparateFilesystems;
    impl Statvfs for SeparateFilesystems {
        fn statvfs(&self, path: &Path) -> io::Result<FsStats> {
            match path.to_str() {
                Some("/podman/graph") => Ok(FsStats::from_blocks(1, 10_000, 10_000, 9_000)),
                Some("/podman/volumes") => Ok(FsStats::from_blocks(1, 1_000, 400, 200)),
                _ => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            }
        }
    }

    #[test]
    fn podman_capacity_is_bounded_by_the_actual_volume_mount() {
        for info in [
            json!({"Store":{"GraphRoot":"/podman/graph","VolumePath":"/podman/volumes"}}),
            json!({"store":{"graphRoot":"/podman/graph","volumePath":"/podman/volumes"}}),
        ] {
            assert_eq!(storage_headroom(&info, &SeparateFilesystems), Ok(200));
        }
        let unmeasurable = json!({"DockerRootDir":"/unmeasurable"});
        assert_eq!(
            storage_headroom(&unmeasurable, &SeparateFilesystems),
            Err(PlacementRefusal::StorageUnmeasurable)
        );
    }

    #[derive(Default)]
    struct Podman(Mutex<Vec<String>>);
    impl DockerTransport for Podman {
        async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
            assert_eq!(request.method, DockerMethod::Get);
            let body = match request.path.as_str() {
                "/v1.44/info" => json!({
                    "BuildahVersion": "1.41",
                    "CgroupVersion": "2",
                    "CgroupDriver": "systemd",
                    "SecurityOptions": ["name=rootless"],
                    "DockerRootDir": "/podman/graph"
                }),
                "/v4.0.0/libpod/info" => json!({
                    "store": {"graphRoot": "/podman/graph", "volumePath": "/podman/volumes"}
                }),
                path => panic!("unexpected request {path}"),
            };
            self.0.lock().unwrap().push(request.path);
            Ok(DockerResponse {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn podman_compatibility_info_is_followed_by_the_storage_layout() {
        let podman = Podman::default();
        let outcome = check_placement(
            &podman,
            "/v1.44",
            Duration::from_secs(1),
            300,
            &SeparateFilesystems,
        )
        .await;
        assert_eq!(
            outcome,
            Err(PlacementRefusal::DiskHeadroom {
                available: 200,
                minimum: 300,
            })
        );
        assert_eq!(
            *podman.0.lock().unwrap(),
            ["/v1.44/info", "/v4.0.0/libpod/info"]
        );
    }
}
