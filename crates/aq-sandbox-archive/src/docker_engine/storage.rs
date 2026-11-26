use super::{
    CallError, DockerMethod, DockerRequest, DockerTransport, call, daemon_is_rootless, graph_root,
    require_enforced_limits,
};
use serde_json::Value;
use std::sync::Arc;
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
    /// The server has not sent the node's headroom Setting yet.
    #[error("sandbox_headroom_unreported")]
    HeadroomUnreported,
}
impl PlacementRefusal {
    /// The refusal's code without its numbers, and for a disk shortfall the bytes
    /// available and required.
    pub fn code(&self) -> (String, Option<(u64, u64)>) {
        match *self {
            Self::DiskHeadroom { available, minimum } => (
                "sandbox_disk_headroom_unavailable".into(),
                Some((available, minimum)),
            ),
            ref other => (other.to_string(), None),
        }
    }
}

impl From<PlacementRefusal> for aq_machine_load::AdmissionDeferral {
    fn from(refusal: PlacementRefusal) -> Self {
        let (code, bytes) = refusal.code();
        Self::new(code, bytes)
    }
}

/// A disk shortfall first reclaims what `reclaim` can (stale build-cache keys), then checks
/// again when it freed anything; every other answer stands.
pub async fn admit_reclaiming<C, F, R, G>(check: C, reclaim: R) -> Result<(), PlacementRefusal>
where
    C: Fn() -> F,
    F: std::future::Future<Output = Result<(), PlacementRefusal>>,
    R: FnOnce() -> G,
    G: std::future::Future<Output = u64>,
{
    match check().await {
        Err(PlacementRefusal::DiskHeadroom { .. }) if reclaim().await > 0 => check().await,
        other => other,
    }
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
    let storage = read_daemon_storage(transport, api_version, timeout).await?;
    storage.admit(statvfs, minimum)
}

/// What placement needs of a qualified (rootless, limit-enforcing) daemon: its overlay storage
/// root and, when mounted apart, its work-volume directory. They change only with the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonStorage {
    pub graph: std::path::PathBuf,
    pub volumes: Option<std::path::PathBuf>,
}

/// Reads [`DaemonStorage`] from `/info` (and Podman's libpod `/info`).
pub async fn read_daemon_storage(
    transport: &impl DockerTransport,
    api_version: &str,
    timeout: Duration,
) -> Result<DaemonStorage, PlacementRefusal> {
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
    storage_roots(&info)
}

impl DaemonStorage {
    /// The free bytes of the smaller of its filesystems against `minimum`.
    pub fn admit(&self, statvfs: &dyn Statvfs, minimum: u64) -> Result<(), PlacementRefusal> {
        let unknown = PlacementRefusal::StorageUnmeasurable;
        let free = |path: &Path| {
            statvfs
                .statvfs(path)
                .map(|fs| fs.free_bytes)
                .map_err(|_| unknown)
        };
        let mut available = free(&self.graph)?;
        if let Some(volumes) = &self.volumes {
            available = available.min(free(volumes)?);
        }
        if available < minimum {
            return Err(PlacementRefusal::DiskHeadroom { available, minimum });
        }
        Ok(())
    }
}

/// The node's [`DaemonStorage`], read once and shared by every slot: each check is a statvfs
/// of the cached roots. It is read again when the daemon's socket is a new file (the daemon
/// restarted) or after a check that could not measure the roots. Runtime state: any two
/// compare equal, so a config compares as before.
#[derive(Clone, Debug, Default)]
pub struct DaemonCache(Arc<tokio::sync::Mutex<Option<(SocketFile, DaemonStorage)>>>);
/// A socket file's device, inode and change time.
type SocketFile = (u64, u64, i64, i64);
impl PartialEq for DaemonCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for DaemonCache {}
impl DaemonCache {
    /// Admits against the cached roots of the daemon behind `socket`, reading them with
    /// `read` only when none are cached for that socket file. Concurrent checks share one read.
    pub async fn admit<F>(
        &self,
        socket: &Path,
        read: F,
        statvfs: &dyn Statvfs,
        minimum: u64,
    ) -> Result<(), PlacementRefusal>
    where
        F: std::future::Future<Output = Result<DaemonStorage, PlacementRefusal>>,
    {
        use std::os::unix::fs::MetadataExt;
        let socket = std::fs::metadata(socket).map_err(|_| PlacementRefusal::DaemonUnavailable)?;
        // A recreated socket has a new change time even when its inode number is reused.
        let key = (
            socket.dev(),
            socket.ino(),
            socket.ctime(),
            socket.ctime_nsec(),
        );
        let mut cached = self.0.lock().await;
        if cached.as_ref().is_none_or(|(known, _)| *known != key) {
            *cached = Some((key, read.await?));
        }
        let storage = cached
            .as_ref()
            .map(|(_, s)| s.clone())
            .ok_or(PlacementRefusal::DaemonUnavailable)?;
        let admitted = storage.admit(statvfs, minimum);
        if admitted == Err(PlacementRefusal::StorageUnmeasurable) {
            *cached = None;
        }
        admitted
    }
    /// [`Self::admit`] against the daemon on Unix socket `socket`, with the node's headroom.
    pub async fn admit_socket(
        &self,
        socket: &Path,
        api_version: &str,
        timeout: Duration,
        limits: &crate::LiveLimits,
    ) -> Result<(), PlacementRefusal> {
        super::validate_socket(socket).map_err(|_| PlacementRefusal::DaemonUnavailable)?;
        let limits = limits.get().ok_or(PlacementRefusal::HeadroomUnreported)?;
        let transport = super::UnixSocketTransport::new(socket.to_owned());
        let read = read_daemon_storage(&transport, api_version, timeout);
        let minimum = limits.sandbox_headroom_bytes.max(1);
        self.admit(socket, read, &HostStatvfs, minimum).await
    }
    /// The cached overlay storage root, else the one `info` (a daemon `/info`) names.
    pub async fn graph_root_or(
        &self,
        info: impl std::future::Future<Output = Option<Value>>,
    ) -> Option<std::path::PathBuf> {
        if let Some((_, storage)) = self.0.lock().await.as_ref() {
            return Some(storage.graph.clone());
        }
        Some(graph_root(&info.await?)?.to_owned())
    }
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

fn storage_roots(info: &Value) -> Result<DaemonStorage, PlacementRefusal> {
    let unknown = PlacementRefusal::StorageUnmeasurable;
    let graph = graph_root(info).ok_or(unknown)?;
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
    Ok(DaemonStorage {
        graph: graph.to_owned(),
        volumes: volumes.filter(|v| *v != graph).map(Path::to_owned),
    })
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
            let storage = storage_roots(&info).unwrap();
            assert_eq!(storage.admit(&SeparateFilesystems, 200), Ok(()));
            let short = PlacementRefusal::DiskHeadroom {
                available: 200,
                minimum: 201,
            };
            assert_eq!(storage.admit(&SeparateFilesystems, 201), Err(short));
        }
        let unmeasurable = json!({"DockerRootDir":"/unmeasurable"});
        assert_eq!(
            storage_roots(&unmeasurable)
                .unwrap()
                .admit(&SeparateFilesystems, 1),
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

    #[tokio::test]
    async fn a_disk_shortfall_reclaims_then_checks_again() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let short = PlacementRefusal::DiskHeadroom {
            available: 1,
            minimum: 2,
        };
        // Free space appears once reclaim freed something: the second check admits.
        let checks = AtomicU64::new(0);
        let check = || {
            let n = checks.fetch_add(1, Ordering::SeqCst);
            async move { if n == 0 { Err(short) } else { Ok(()) } }
        };
        assert_eq!(admit_reclaiming(&check, || async { 5 }).await, Ok(()));
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        // Nothing reclaimed: no second check, the shortfall stands.
        let checks = AtomicU64::new(0);
        let check = || {
            checks.fetch_add(1, Ordering::SeqCst);
            async move { Err(short) }
        };
        assert_eq!(admit_reclaiming(&check, || async { 0 }).await, Err(short));
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        // Another refusal never reclaims.
        let reclaimed = || async { panic!("reclaimed on a daemon refusal") };
        let daemon = || async { Err(PlacementRefusal::DaemonUnavailable) };
        assert_eq!(
            admit_reclaiming(daemon, reclaimed).await,
            Err(PlacementRefusal::DaemonUnavailable)
        );
        assert_eq!(
            short.code(),
            ("sandbox_disk_headroom_unavailable".into(), Some((1, 2)))
        );
    }

    /// The podman socket is any existing file; a new file is a restarted daemon.
    fn socket(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("podman.sock");
        std::fs::write(&path, "").unwrap();
        path
    }

    #[tokio::test]
    async fn many_polls_read_the_daemon_once_and_a_restart_reads_it_again() {
        let (podman, cache) = (Podman::default(), DaemonCache::default());
        let dir = tempfile::tempdir().unwrap();
        let path = socket(dir.path());
        let read = || read_daemon_storage(&podman, "/v1.44", Duration::from_secs(1));
        let info = ["/v1.44/info", "/v4.0.0/libpod/info"];
        for minimum in [1, 100, 200] {
            let admitted = cache.admit(&path, read(), &SeparateFilesystems, minimum);
            assert_eq!(admitted.await, Ok(()));
        }
        let short = cache.admit(&path, read(), &SeparateFilesystems, 300).await;
        assert!(matches!(
            short,
            Err(PlacementRefusal::DiskHeadroom { available: 200, .. })
        ));
        assert_eq!(*podman.0.lock().unwrap(), info, "four polls, one read");
        let graph = cache.graph_root_or(async { panic!("the root is cached") });
        assert_eq!(graph.await.unwrap(), Path::new("/podman/graph"));
        // A restarted daemon has a new socket file: its roots are read again, once.
        std::fs::rename(&path, dir.path().join("stopped.sock")).unwrap();
        let path = socket(dir.path());
        for _ in 0..3 {
            assert_eq!(
                cache.admit(&path, read(), &SeparateFilesystems, 1).await,
                Ok(())
            );
        }
        assert_eq!(podman.0.lock().unwrap().len(), 4);
        // No socket: the daemon is unavailable and nothing is read.
        std::fs::remove_file(&path).unwrap();
        let gone = cache.admit(&path, read(), &SeparateFilesystems, 1).await;
        assert_eq!(gone, Err(PlacementRefusal::DaemonUnavailable));
        assert_eq!(podman.0.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn roots_that_cannot_be_measured_are_read_again() {
        struct Docker(Mutex<u32>);
        impl DockerTransport for Docker {
            async fn send(&self, _: DockerRequest) -> Result<DockerResponse, TransportError> {
                *self.0.lock().unwrap() += 1;
                let body = json!({"CgroupVersion": "2", "CgroupDriver": "systemd",
                    "SecurityOptions": ["name=rootless"], "DockerRootDir": "/unmeasurable"});
                Ok(DockerResponse {
                    status: 200,
                    body: serde_json::to_vec(&body).unwrap(),
                })
            }
        }
        let (docker, cache) = (Docker(Mutex::new(0)), DaemonCache::default());
        let dir = tempfile::tempdir().unwrap();
        let path = socket(dir.path());
        for _ in 0..2 {
            let read = read_daemon_storage(&docker, "/v1.44", Duration::from_secs(1));
            let admitted = cache.admit(&path, read, &SeparateFilesystems, 1).await;
            assert_eq!(admitted, Err(PlacementRefusal::StorageUnmeasurable));
        }
        assert_eq!(*docker.0.lock().unwrap(), 2);
    }
}
