//! Admission checks for host Unix sockets and container paths.
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
};

/// Host Docker daemon sockets that must never be used for sandbox work.
pub const MAIN_DAEMON_SOCKETS: &[&str] = &["/var/run/docker.sock", "/run/docker.sock"];

pub fn validate_socket(socket: &Path) -> Result<(), &'static str> {
    if !socket.is_absolute() {
        return Err("sandbox daemon socket must be an absolute path");
    }
    if is_main_daemon_path(socket) {
        return Err("refusing the host's main Docker daemon");
    }
    let canonical =
        std::fs::canonicalize(socket).map_err(|_| "sandbox daemon socket unavailable")?;
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| "sandbox daemon socket unavailable")?;
    if is_main_daemon_path(&canonical) || same_file_as_main_daemon(&metadata) {
        return Err("refusing the host's main Docker daemon");
    }
    if !metadata.file_type().is_socket() {
        return Err("sandbox daemon socket is not a Unix socket");
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("sandbox daemon socket is not owned by the worker user");
    }
    Ok(())
}

pub fn exposes_daemon_socket(socket: &Path, host_path: &Path) -> bool {
    let canonical = std::fs::canonicalize(host_path).unwrap_or_else(|_| host_path.into());
    let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.into());
    is_main_daemon_path(host_path)
        || is_main_daemon_path(&canonical)
        || socket.starts_with(&canonical)
        || std::fs::metadata(&canonical)
            .is_ok_and(|m| m.file_type().is_socket() || same_file_as_main_daemon(&m))
}
fn is_main_daemon_path(path: &Path) -> bool {
    MAIN_DAEMON_SOCKETS
        .iter()
        .any(|main| path == Path::new(main))
}

fn same_file_as_main_daemon(metadata: &std::fs::Metadata) -> bool {
    MAIN_DAEMON_SOCKETS.iter().any(|main| {
        std::fs::metadata(main)
            .is_ok_and(|m| m.dev() == metadata.dev() && m.ino() == metadata.ino())
    })
}

/// Absolute, normalized and under `/aq/`.
pub fn container_path_ok(path: &str) -> bool {
    path.strip_prefix("/aq/").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
    })
}

/// True when `path` is `base` or below it.
pub fn under(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("/", |(parent, _)| parent)
}
