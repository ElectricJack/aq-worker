//! Positive kernel exec evidence. EOF is only a trigger to inspect, never proof.
use super::*;
use std::{ffi::CString, os::fd::AsRawFd, os::unix::ffi::OsStrExt};

pub(crate) fn observed_image(process: &ProcessEvidence, image: &ElfIdentity) -> Result<()> {
    let fd = ownership::verified_pidfd(process)?;
    ensure!(ownership::pidfd_is_live(&fd)?, "exec process not live");
    let metadata = std::fs::metadata(format!("/proc/{}/exe", process.pid))?;
    ensure!(
        metadata.dev() == image.device && metadata.ino() == image.inode,
        "live executable does not match sealed image"
    );
    ensure!(
        ownership::process_evidence(process.pid)? == *process && ownership::pidfd_is_live(&fd)?,
        "exec process changed while observed"
    );
    Ok(())
}
/// Called only in the dedicated pane helper, after the guardian release packet.
/// No path lookup, shell fallback or inherited ambient environment.
pub(crate) fn enter(
    file: &File,
    image: &ElfIdentity,
    command: &LocalCommand,
    directory: &Path,
    workspace: &Path,
) -> Result<()> {
    executable::verify_file(file, image)?;
    command.validate()?;
    let mut args = vec![CString::new(command.executable.as_os_str().as_bytes())?];
    for arg in &command.arguments {
        args.push(CString::new(arg.as_bytes())?);
    }
    let mut environment = std::collections::BTreeMap::from([
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        (
            "AQ_SESSION_DIRECTORY".to_owned(),
            directory
                .to_str()
                .context("session directory UTF-8")?
                .to_owned(),
        ),
    ]);
    environment.extend(command.environment.clone());
    let environment: Vec<_> = environment
        .into_iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")))
        .collect::<std::result::Result<_, _>>()?;
    let mut argv: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<_> = environment.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    std::env::set_current_dir(workspace)?;
    let empty = c"";
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            file.as_raw_fd(),
            empty.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    Err(std::io::Error::last_os_error()).context("sealed FD native exec failed; no fallback")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};
    #[test]
    fn eof_without_matching_live_image_is_not_exec_proof() {
        let process = ownership::process_evidence(std::process::id()).unwrap();
        let mut image = ElfIdentity {
            artifact: ArtifactRef {
                id: aq_session_protocol::Text::new("synthetic").unwrap(),
                version: aq_session_protocol::Text::new("1").unwrap(),
                digest: Digest::of(b"not current image"),
            },
            bytes: 128,
            device: 0,
            inode: 1,
        };
        assert!(observed_image(&process, &image).is_err());
        let metadata = std::fs::metadata("/proc/self/exe").unwrap();
        image.device = metadata.dev();
        image.inode = metadata.ino();
        assert!(observed_image(&process, &image).is_ok());
        let stale = ProcessEvidence {
            pid: process.pid,
            start_ticks: process.start_ticks + 1,
        };
        assert!(observed_image(&stale, &image).is_err());
        // A closed CLOEXEC-shaped channel alone provides no executable evidence.
        let mut pair = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let a = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let b = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        drop(b);
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(a.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
        image.inode += 1;
        assert!(observed_image(&process, &image).is_err());
    }
}
